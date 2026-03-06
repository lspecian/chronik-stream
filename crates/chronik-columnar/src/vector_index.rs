//! Vector Index Manager
//!
//! Manages HNSW indexes per topic-partition, integrating embedding generation
//! with the vector search infrastructure.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    VectorIndexManager                           │
//! ├─────────────────────────────────────────────────────────────────┤
//! │                                                                 │
//! │  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐     │
//! │  │ topic-0      │    │ topic-1      │    │ topic-N      │     │
//! │  │ ┌──────────┐ │    │ ┌──────────┐ │    │ ┌──────────┐ │     │
//! │  │ │ HNSW[0]  │ │    │ │ HNSW[0]  │ │    │ │ HNSW[0]  │ │     │
//! │  │ ├──────────┤ │    │ ├──────────┤ │    │ ├──────────┤ │     │
//! │  │ │ HNSW[1]  │ │    │ │ HNSW[1]  │ │    │ │ HNSW[1]  │ │     │
//! │  │ ├──────────┤ │    │ └──────────┘ │    │ ├──────────┤ │     │
//! │  │ │ HNSW[2]  │ │    │              │    │ │ HNSW[2]  │ │     │
//! │  │ └──────────┘ │    │              │    │ └──────────┘ │     │
//! │  └──────────────┘    └──────────────┘    └──────────────┘     │
//! │                                                                 │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

use anyhow::{anyhow, Result};
use instant_distance::{Builder, Search, HnswMap};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration, Instant};
use tracing::{debug, error, info, warn};

use chronik_embeddings::config::{HnswConfig as EmbeddingHnswConfig, QuantizationMode, VectorSearchConfig};
use chronik_embeddings::provider::EmbeddingProvider;
use chronik_monitoring::unified_metrics::MetricsRecorder;

/// Configuration for the vector index manager
#[derive(Debug, Clone)]
pub struct VectorIndexConfig {
    /// Base path for storing vector indexes
    pub base_path: PathBuf,
    /// Default HNSW configuration for new indexes
    pub default_hnsw_config: HnswIndexConfig,
    /// Maximum vectors in memory before flushing to disk
    pub max_vectors_in_memory: usize,
    /// Interval for periodic index snapshots (in seconds)
    pub snapshot_interval_secs: u64,
}

impl Default for VectorIndexConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("./data/vectors"),
            default_hnsw_config: HnswIndexConfig::default(),
            max_vectors_in_memory: 100_000,
            snapshot_interval_secs: 300, // 5 minutes
        }
    }
}

/// HNSW-specific configuration (mirrors chronik-storage's HnswConfig)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswIndexConfig {
    /// Number of dimensions (full embedding dimensionality)
    pub dimensions: usize,
    /// Distance metric
    pub metric: DistanceMetric,
    /// M parameter (bi-directional links per node)
    pub m: usize,
    /// ef_construction (candidate list size during build)
    pub ef_construction: usize,
    /// ef_search (candidate list size during search)
    pub ef_search: usize,
    /// VO-3: Vector quantization mode
    #[serde(default)]
    pub quantization: QuantizationMode,
    /// VO-6: Search dimensions for Matryoshka truncation.
    /// 0 = use full `dimensions`. When set, HNSW graph is built on truncated vectors
    /// but full vectors are stored for optional adaptive re-scoring.
    #[serde(default)]
    pub search_dimensions: usize,
    /// VO-6: Enable adaptive two-phase retrieval (overretrieve at search_dims, re-score at full dims)
    #[serde(default)]
    pub adaptive_retrieval: bool,
}

impl Default for HnswIndexConfig {
    fn default() -> Self {
        Self {
            dimensions: 1536, // OpenAI text-embedding-3-small
            metric: DistanceMetric::Cosine,
            m: 16,
            ef_construction: 200,
            ef_search: 50,
            quantization: QuantizationMode::F32,
            search_dimensions: 0, // 0 = use full dimensions
            adaptive_retrieval: false,
        }
    }
}

impl HnswIndexConfig {
    /// Create from VectorSearchConfig (which has both HnswConfig and dimensions)
    pub fn from_vector_config(config: &VectorSearchConfig) -> Self {
        let metric = match config.hnsw.metric {
            chronik_embeddings::config::DistanceMetric::Cosine => DistanceMetric::Cosine,
            chronik_embeddings::config::DistanceMetric::Euclidean => DistanceMetric::Euclidean,
            chronik_embeddings::config::DistanceMetric::InnerProduct => DistanceMetric::DotProduct,
        };

        Self {
            dimensions: config.embedding.dimensions,
            metric,
            m: config.hnsw.m,
            ef_construction: config.hnsw.ef_construction,
            ef_search: config.hnsw.ef_search,
            quantization: config.quantization,
            search_dimensions: config.search_dimensions,
            adaptive_retrieval: config.adaptive_retrieval,
        }
    }

    /// Create from EmbeddingHnswConfig with explicit dimensions
    pub fn from_hnsw_config(config: &EmbeddingHnswConfig, dimensions: usize) -> Self {
        let metric = match config.metric {
            chronik_embeddings::config::DistanceMetric::Cosine => DistanceMetric::Cosine,
            chronik_embeddings::config::DistanceMetric::Euclidean => DistanceMetric::Euclidean,
            chronik_embeddings::config::DistanceMetric::InnerProduct => DistanceMetric::DotProduct,
        };

        Self {
            dimensions,
            metric,
            m: config.m,
            ef_construction: config.ef_construction,
            ef_search: config.ef_search,
            quantization: QuantizationMode::F32,
            search_dimensions: 0,
            adaptive_retrieval: false,
        }
    }

    /// VO-6: Get effective search dimensions (0 means use full dimensions).
    pub fn effective_search_dims(&self) -> usize {
        if self.search_dimensions > 0 && self.search_dimensions < self.dimensions {
            self.search_dimensions
        } else {
            self.dimensions
        }
    }

    /// VO-6: Whether Matryoshka dimension reduction is active.
    pub fn is_matryoshka_enabled(&self) -> bool {
        self.search_dimensions > 0 && self.search_dimensions < self.dimensions
    }
}

/// Distance metrics for vector similarity
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum DistanceMetric {
    /// Euclidean distance (L2)
    Euclidean,
    /// Cosine similarity (1 - cosine)
    Cosine,
    /// Dot product (negated for minimization)
    DotProduct,
}

/// Key for identifying a topic-partition index.
///
/// VO-5: Extended with `model_id` to support multiple embedding models per topic.
/// The default model is `"default"` for backward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicPartitionKey {
    pub topic: String,
    pub partition: i32,
    /// VO-5: Embedding model identifier. Default: `"default"`.
    pub model_id: String,
}

/// Default model ID used when no specific model is requested.
pub const DEFAULT_MODEL_ID: &str = "default";

impl TopicPartitionKey {
    /// Create a key with the default model.
    pub fn new(topic: impl Into<String>, partition: i32) -> Self {
        Self {
            topic: topic.into(),
            partition,
            model_id: DEFAULT_MODEL_ID.to_string(),
        }
    }

    /// VO-5: Create a key for a specific model.
    pub fn with_model(topic: impl Into<String>, partition: i32, model_id: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            partition,
            model_id: model_id.into(),
        }
    }
}

/// A vector entry with metadata
#[derive(Debug, Clone)]
pub struct VectorEntry {
    /// Message offset as the vector ID
    pub offset: i64,
    /// The embedding vector
    pub vector: Vec<f32>,
    /// Original text that was embedded (for debugging/logging)
    pub text_preview: Option<String>,
}

/// Statistics for a topic-partition index
#[derive(Debug, Clone, Default)]
pub struct PartitionIndexStats {
    /// Number of vectors in the index
    pub vector_count: usize,
    /// Number of vectors pending build
    pub pending_count: usize,
    /// Whether the index has been built
    pub is_built: bool,
    /// Last offset indexed
    pub last_offset: Option<i64>,
    /// Dimensions of vectors
    pub dimensions: usize,
    /// VO-5: Embedding model ID
    pub model_id: String,
}

/// Statistics for a topic across all partitions and models
#[derive(Debug, Clone, Default)]
pub struct TopicIndexStats {
    /// Number of partitions with indexes
    pub partition_count: usize,
    /// Total vectors across all partitions
    pub total_vectors: usize,
    /// Dimensions of vectors
    pub dimensions: usize,
    /// Per-partition statistics (aggregated across models for backward compat)
    pub partitions: HashMap<i32, PartitionIndexStats>,
    /// Whether vector indexes are still loading from disk (warmup phase)
    pub loading: bool,
    /// VO-5: Model IDs present in this topic's indexes
    pub models: Vec<String>,
}

// ============================================================================
// HNSW Point Wrappers for instant-distance
// ============================================================================

/// Point wrapper for Euclidean distance (L2)
#[derive(Clone)]
struct EuclideanPoint(Vec<f32>);

impl instant_distance::Point for EuclideanPoint {
    fn distance(&self, other: &Self) -> f32 {
        self.0.iter()
            .zip(other.0.iter())
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            .sqrt()
    }
}

/// Point wrapper for Cosine distance
#[derive(Clone)]
struct CosinePoint(Vec<f32>);

impl instant_distance::Point for CosinePoint {
    fn distance(&self, other: &Self) -> f32 {
        let dot: f32 = self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum();
        let norm_a: f32 = self.0.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
        let norm_b: f32 = other.0.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            1.0  // Maximum distance for zero vectors
        } else {
            1.0 - (dot / (norm_a * norm_b))
        }
    }
}

/// Point wrapper for Dot Product (negated for minimization)
#[derive(Clone)]
struct DotProductPoint(Vec<f32>);

impl instant_distance::Point for DotProductPoint {
    fn distance(&self, other: &Self) -> f32 {
        // Negate because instant-distance minimizes distance
        -self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum::<f32>()
    }
}

// ============================================================================
// VO-6: Matryoshka Vector Truncation
// ============================================================================

/// Truncate a vector to the first `dims` dimensions.
///
/// Returns a clone if dims >= vec.len() (no truncation needed).
/// Used for Matryoshka dimension reduction where the first N dims
/// of OpenAI text-embedding-3 models retain most semantic information.
#[inline]
fn truncate_vec(vec: &[f32], dims: usize) -> Vec<f32> {
    if dims >= vec.len() {
        vec.to_vec()
    } else {
        vec[..dims].to_vec()
    }
}

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk backwards from max_bytes to find a valid char boundary
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ============================================================================
// VO-3: Scalar Quantization
// ============================================================================

/// Scalar quantizer for compressing f32 vectors to uint8 or f16.
///
/// For uint8 mode, each dimension is linearly mapped from [min, max] → [0, 255]
/// using per-dimension min/max statistics computed from the first batch of vectors.
///
/// Asymmetric distance: queries stay f32, database vectors are quantized.
/// This preserves ~97-98% recall for cosine similarity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalarQuantizer {
    /// Per-dimension minimum values (for uint8 de-normalization)
    pub min_vals: Vec<f32>,
    /// Per-dimension maximum values (for uint8 de-normalization)
    pub max_vals: Vec<f32>,
    /// Number of dimensions
    pub dimensions: usize,
}

impl ScalarQuantizer {
    /// Create a new quantizer from a set of training vectors.
    ///
    /// Computes per-dimension min/max from the provided vectors.
    /// At least one vector must be provided.
    pub fn fit(vectors: &[(u64, Vec<f32>)], dimensions: usize) -> Self {
        if vectors.is_empty() {
            return Self {
                min_vals: vec![0.0; dimensions],
                max_vals: vec![1.0; dimensions],
                dimensions,
            };
        }

        let mut min_vals = vec![f32::MAX; dimensions];
        let mut max_vals = vec![f32::MIN; dimensions];

        for (_, vec) in vectors {
            for (i, &val) in vec.iter().enumerate().take(dimensions) {
                if val < min_vals[i] { min_vals[i] = val; }
                if val > max_vals[i] { max_vals[i] = val; }
            }
        }

        // Avoid zero-range dimensions (would cause division by zero)
        for i in 0..dimensions {
            if (max_vals[i] - min_vals[i]).abs() < f32::EPSILON {
                max_vals[i] = min_vals[i] + 1.0;
            }
        }

        Self { min_vals, max_vals, dimensions }
    }

    /// Quantize an f32 vector to uint8.
    pub fn quantize_uint8(&self, vector: &[f32]) -> Vec<u8> {
        vector.iter().enumerate().map(|(i, &val)| {
            let min = self.min_vals[i];
            let range = self.max_vals[i] - min;
            let normalized = ((val - min) / range).clamp(0.0, 1.0);
            (normalized * 255.0) as u8
        }).collect()
    }

    /// Dequantize a uint8 vector back to approximate f32.
    pub fn dequantize_uint8(&self, quantized: &[u8]) -> Vec<f32> {
        quantized.iter().enumerate().map(|(i, &val)| {
            let min = self.min_vals[i];
            let range = self.max_vals[i] - min;
            min + (val as f32 / 255.0) * range
        }).collect()
    }

    /// Asymmetric cosine distance: f32 query vs uint8 database vector.
    ///
    /// Dequantizes on-the-fly per dimension — avoids full dequantize allocation.
    pub fn asymmetric_cosine_distance(&self, query: &[f32], quantized: &[u8]) -> f32 {
        let mut dot = 0.0f32;
        let mut norm_q = 0.0f32;
        let mut norm_d = 0.0f32;

        for i in 0..query.len().min(quantized.len()) {
            let q = query[i];
            let d = self.min_vals[i] + (quantized[i] as f32 / 255.0) * (self.max_vals[i] - self.min_vals[i]);
            dot += q * d;
            norm_q += q * q;
            norm_d += d * d;
        }

        let norm_q = norm_q.sqrt();
        let norm_d = norm_d.sqrt();
        if norm_q == 0.0 || norm_d == 0.0 {
            1.0
        } else {
            1.0 - (dot / (norm_q * norm_d))
        }
    }

    /// Asymmetric euclidean distance: f32 query vs uint8 database vector.
    pub fn asymmetric_euclidean_distance(&self, query: &[f32], quantized: &[u8]) -> f32 {
        let mut sum = 0.0f32;
        for i in 0..query.len().min(quantized.len()) {
            let d = self.min_vals[i] + (quantized[i] as f32 / 255.0) * (self.max_vals[i] - self.min_vals[i]);
            let diff = query[i] - d;
            sum += diff * diff;
        }
        sum.sqrt()
    }

    /// Asymmetric dot product distance: f32 query vs uint8 database vector.
    pub fn asymmetric_dot_distance(&self, query: &[f32], quantized: &[u8]) -> f32 {
        let mut dot = 0.0f32;
        for i in 0..query.len().min(quantized.len()) {
            let d = self.min_vals[i] + (quantized[i] as f32 / 255.0) * (self.max_vals[i] - self.min_vals[i]);
            dot += query[i] * d;
        }
        -dot // Negate for minimization
    }

    /// Estimated memory bytes for the quantizer itself.
    pub fn memory_bytes(&self) -> usize {
        // Two Vec<f32> of size dimensions each + usize
        self.dimensions * 4 * 2 + std::mem::size_of::<usize>()
    }
}

// ============================================================================
// Partition Index with HNSW Support
// ============================================================================

/// In-memory vector index for a single partition
///
/// Uses a two-tier architecture (VO-2):
/// - **Built tier**: Vectors in a built HNSW graph for O(log n) search
/// - **Pending tier**: New vectors since last build, searched via brute-force O(m)
///
/// `add_vector()` appends to pending only — does NOT invalidate the built HNSW.
/// `search()` queries both tiers and merges results.
/// `build()` merges pending into built and rebuilds HNSW.
struct PartitionIndex {
    /// HNSW configuration
    config: HnswIndexConfig,
    /// VO-5: Model ID that produced these embeddings (default: "default")
    model_id: String,
    /// Vectors included in the last HNSW build (immutable until next rebuild)
    built_vectors: Vec<(u64, Vec<f32>)>,
    /// New vectors since last HNSW build (searched via brute-force)
    pending_vectors: Vec<(u64, Vec<f32>)>,
    /// VO-3: Quantized copies of built_vectors (uint8 mode only)
    quantized_built: Vec<(u64, Vec<u8>)>,
    /// VO-3: Quantized copies of pending_vectors (uint8 mode only)
    quantized_pending: Vec<(u64, Vec<u8>)>,
    /// VO-3: Scalar quantizer (trained from built_vectors, used for uint8)
    quantizer: Option<ScalarQuantizer>,
    /// Last offset indexed (across both tiers)
    last_offset: Option<i64>,
    /// Built HNSW index for Euclidean distance
    euclidean_index: Option<HnswMap<EuclideanPoint, u64>>,
    /// Built HNSW index for Cosine distance
    cosine_index: Option<HnswMap<CosinePoint, u64>>,
    /// Built HNSW index for Dot Product distance
    dot_index: Option<HnswMap<DotProductPoint, u64>>,
    /// Offsets that already have vectors (dedup across multiple loads/re-ingests)
    known_offsets: std::collections::HashSet<u64>,
}

/// Thresholds for triggering HNSW rebuild
const REBUILD_PENDING_ABSOLUTE: usize = 5000;
const REBUILD_PENDING_RATIO_PERCENT: usize = 10;
/// Minimum vectors before first HNSW build (when no graph exists at all)
const REBUILD_FIRST_BUILD_MIN: usize = 100;

impl PartitionIndex {
    fn new(config: HnswIndexConfig) -> Self {
        Self::with_model_id(config, "default".to_string())
    }

    /// VO-5: Create a partition index for a specific model.
    fn with_model_id(config: HnswIndexConfig, model_id: String) -> Self {
        Self {
            config,
            model_id,
            built_vectors: Vec::new(),
            pending_vectors: Vec::new(),
            quantized_built: Vec::new(),
            quantized_pending: Vec::new(),
            quantizer: None,
            last_offset: None,
            euclidean_index: None,
            cosine_index: None,
            dot_index: None,
            known_offsets: std::collections::HashSet::new(),
        }
    }

    /// Add a vector to the pending buffer.
    ///
    /// VO-2: Does NOT invalidate the built HNSW graph. New vectors are
    /// brute-force searched until the next rebuild merges them in.
    fn add_vector(&mut self, offset: i64, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.config.dimensions {
            return Err(anyhow!(
                "Dimension mismatch: expected {}, got {}",
                self.config.dimensions,
                vector.len()
            ));
        }

        // Dedup: skip if we already have a vector for this offset
        if !self.known_offsets.insert(offset as u64) {
            return Ok(());
        }

        // VO-3: If uint8 quantization and quantizer exists, also store quantized copy
        if matches!(self.config.quantization, QuantizationMode::Uint8) {
            if let Some(ref quantizer) = self.quantizer {
                let quantized = quantizer.quantize_uint8(&vector);
                self.quantized_pending.push((offset as u64, quantized));
            }
        }

        self.pending_vectors.push((offset as u64, vector));
        self.last_offset = Some(offset);

        Ok(())
    }

    /// Total vector count across both tiers.
    fn vector_count(&self) -> usize {
        self.built_vectors.len() + self.pending_vectors.len()
    }

    /// Check if a rebuild is needed based on pending buffer thresholds.
    fn needs_rebuild(&self) -> bool {
        if self.pending_vectors.is_empty() {
            return false;
        }
        // Always rebuild if HNSW graph needs constructing (built_vectors exist without graph)
        if !self.is_built() && !self.built_vectors.is_empty() {
            return true;
        }
        // First build: trigger when no HNSW exists and enough pending vectors accumulated
        if !self.is_built() && self.built_vectors.is_empty() && self.pending_vectors.len() >= REBUILD_FIRST_BUILD_MIN {
            return true;
        }
        // Rebuild when pending exceeds absolute threshold
        if self.pending_vectors.len() >= REBUILD_PENDING_ABSOLUTE {
            return true;
        }
        // Rebuild when pending exceeds ratio of built
        if !self.built_vectors.is_empty()
            && self.pending_vectors.len() * 100 >= self.built_vectors.len() * REBUILD_PENDING_RATIO_PERCENT
        {
            return true;
        }
        false
    }

    /// Build (or rebuild) the HNSW graph.
    ///
    /// VO-2: Merges pending_vectors into built_vectors, then rebuilds the HNSW
    /// graph over all built_vectors. After build, pending_vectors is empty and
    /// the HNSW graph covers every vector.
    fn build(&mut self) -> Result<()> {
        // Nothing to do if no vectors exist at all
        if self.built_vectors.is_empty() && self.pending_vectors.is_empty() {
            return Ok(());
        }

        // Already built and no new pending vectors
        if self.is_built() && self.pending_vectors.is_empty() {
            return Ok(());
        }

        // Merge pending into built (f32 and quantized)
        self.built_vectors.append(&mut self.pending_vectors);
        self.quantized_built.append(&mut self.quantized_pending);

        // Ensure known_offsets is complete after merge
        for (id, _) in &self.built_vectors {
            self.known_offsets.insert(*id);
        }

        // VO-6: Determine HNSW build dimensions (truncated for Matryoshka or full)
        let build_dims = self.config.effective_search_dims();
        let using_matryoshka = self.config.is_matryoshka_enabled();

        debug!(
            "Building HNSW index with {} vectors, {}d{}, metric={:?}",
            self.built_vectors.len(),
            build_dims,
            if using_matryoshka { format!(" (Matryoshka, full={}d)", self.config.dimensions) } else { String::new() },
            self.config.metric
        );

        // Extract IDs for mapping
        let ids: Vec<u64> = self.built_vectors.iter().map(|(id, _)| *id).collect();

        // VO-6: Truncate vectors for HNSW build when Matryoshka is enabled.
        // Full vectors remain in built_vectors for adaptive re-scoring.
        match self.config.metric {
            DistanceMetric::Euclidean => {
                let points: Vec<EuclideanPoint> = self
                    .built_vectors
                    .iter()
                    .map(|(_, v)| EuclideanPoint(truncate_vec(v, build_dims)))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.euclidean_index = Some(hnsw);
            }
            DistanceMetric::Cosine => {
                let points: Vec<CosinePoint> = self
                    .built_vectors
                    .iter()
                    .map(|(_, v)| CosinePoint(truncate_vec(v, build_dims)))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.cosine_index = Some(hnsw);
            }
            DistanceMetric::DotProduct => {
                let points: Vec<DotProductPoint> = self
                    .built_vectors
                    .iter()
                    .map(|(_, v)| DotProductPoint(truncate_vec(v, build_dims)))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.dot_index = Some(hnsw);
            }
        }

        // VO-3: Train quantizer and build quantized copies for uint8 mode
        if matches!(self.config.quantization, QuantizationMode::Uint8) {
            let quantizer = ScalarQuantizer::fit(&self.built_vectors, self.config.dimensions);
            self.quantized_built = self.built_vectors.iter()
                .map(|(id, vec)| (*id, quantizer.quantize_uint8(vec)))
                .collect();
            self.quantizer = Some(quantizer);
            debug!(
                "Trained scalar quantizer for {} vectors (uint8 mode)",
                self.quantized_built.len()
            );
        }

        debug!("HNSW index built successfully");
        Ok(())
    }

    /// Check if the HNSW index has been built
    fn is_built(&self) -> bool {
        match self.config.metric {
            DistanceMetric::Euclidean => self.euclidean_index.is_some(),
            DistanceMetric::Cosine => self.cosine_index.is_some(),
            DistanceMetric::DotProduct => self.dot_index.is_some(),
        }
    }

    fn stats(&self) -> PartitionIndexStats {
        PartitionIndexStats {
            vector_count: self.vector_count(),
            pending_count: self.pending_vectors.len(),
            is_built: self.is_built(),
            last_offset: self.last_offset,
            dimensions: self.config.dimensions,
            model_id: self.model_id.clone(),
        }
    }

    /// VO-3: Estimate total memory usage of vectors in this partition.
    fn vector_memory_bytes(&self) -> usize {
        let f32_bytes = (self.built_vectors.len() + self.pending_vectors.len())
            * self.config.dimensions * 4;
        let quantized_bytes = (self.quantized_built.len() + self.quantized_pending.len())
            * self.config.dimensions;
        let quantizer_bytes = self.quantizer.as_ref().map_or(0, |q| q.memory_bytes());
        f32_bytes + quantized_bytes + quantizer_bytes
    }

    /// Search for k nearest neighbors using two-tier architecture.
    ///
    /// VO-2: Queries built HNSW (O(log n)) + brute-force scan of pending (O(m)),
    /// merges results by distance, and returns top-k.
    ///
    /// VO-6 (Matryoshka): When `search_dimensions < dimensions`, the HNSW graph
    /// is built on truncated vectors, so the query is also truncated for HNSW search.
    /// When `adaptive_retrieval` is enabled, we overretrieve k×3 candidates at
    /// reduced dims, then re-score against full-dimension vectors for final top-k.
    fn search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        if self.built_vectors.is_empty() && self.pending_vectors.is_empty() {
            return Vec::new();
        }

        let using_matryoshka = self.config.is_matryoshka_enabled();
        let adaptive = using_matryoshka && self.config.adaptive_retrieval;
        // Overretrieve when adaptive: get 3x candidates for re-scoring
        let search_k = if adaptive { k * 3 } else { k };

        // VO-6: Truncate query for HNSW search when Matryoshka is active
        let search_query: Vec<f32>;
        let hnsw_query = if using_matryoshka {
            search_query = truncate_vec(query, self.config.effective_search_dims());
            &search_query
        } else {
            query
        };

        let mut results = Vec::new();

        // Tier 1: HNSW search on built vectors (O(log n))
        if self.is_built() {
            results.extend(self.hnsw_search(hnsw_query, search_k));
        } else if !self.built_vectors.is_empty() {
            // Built vectors exist but no HNSW yet — brute-force them
            // Use truncated query for consistency with how vectors would be in HNSW
            results.extend(self.brute_force_search_vectors(hnsw_query, search_k, &self.built_vectors));
        }

        // Tier 2: Brute-force scan of pending vectors (O(m), m typically small)
        // VO-3: Use asymmetric quantized distance when quantizer is available
        if !self.pending_vectors.is_empty() {
            if matches!(self.config.quantization, QuantizationMode::Uint8) && !self.quantized_pending.is_empty() {
                if let Some(ref quantizer) = self.quantizer {
                    results.extend(self.brute_force_search_quantized(hnsw_query, search_k, &self.quantized_pending, quantizer));
                } else {
                    results.extend(self.brute_force_search_vectors(hnsw_query, search_k, &self.pending_vectors));
                }
            } else {
                results.extend(self.brute_force_search_vectors(hnsw_query, search_k, &self.pending_vectors));
            }
        }

        // VO-6 adaptive Phase 2: Re-score candidates using full-dimension vectors
        if adaptive && !results.is_empty() {
            results = self.rescore_full_dimensions(query, &results, k);
        } else {
            // Standard: sort by distance and truncate
            results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            results.truncate(k);
        }

        // Un-negate DotProduct scores for callers (convert distance back to similarity)
        if matches!(self.config.metric, DistanceMetric::DotProduct) {
            for r in &mut results {
                r.1 = -r.1;
            }
        }

        results
    }

    /// VO-6: Re-score candidate results using full-dimension vectors.
    ///
    /// Given candidates found via truncated-dimension search, looks up the full
    /// vector for each candidate and recomputes the distance at full dimensionality.
    /// Returns top-k by full-dimension distance.
    fn rescore_full_dimensions(
        &self,
        query: &[f32],
        candidates: &[(i64, f32)],
        k: usize,
    ) -> Vec<(i64, f32)> {
        // Build a map of offset → full vector for quick lookup
        let mut rescored: Vec<(i64, f32)> = candidates
            .iter()
            .filter_map(|(offset, _approx_dist)| {
                let uid = *offset as u64;
                // Look up full vector in built_vectors or pending_vectors
                let full_vec = self.built_vectors.iter()
                    .find(|(id, _)| *id == uid)
                    .or_else(|| self.pending_vectors.iter().find(|(id, _)| *id == uid));

                full_vec.map(|(_, vec)| {
                    let dist = self.compute_distance(query, vec);
                    (*offset, dist)
                })
            })
            .collect();

        rescored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        rescored.truncate(k);
        rescored
    }

    /// HNSW-based O(log n) search
    fn hnsw_search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        let mut search = Search::default();
        let mut results = Vec::with_capacity(k);

        match self.config.metric {
            DistanceMetric::Euclidean => {
                if let Some(ref hnsw) = self.euclidean_index {
                    let query_point = EuclideanPoint(query.to_vec());
                    for candidate in hnsw.search(&query_point, &mut search).take(k) {
                        results.push((*candidate.value as i64, candidate.distance));
                    }
                }
            }
            DistanceMetric::Cosine => {
                if let Some(ref hnsw) = self.cosine_index {
                    let query_point = CosinePoint(query.to_vec());
                    for candidate in hnsw.search(&query_point, &mut search).take(k) {
                        results.push((*candidate.value as i64, candidate.distance));
                    }
                }
            }
            DistanceMetric::DotProduct => {
                if let Some(ref hnsw) = self.dot_index {
                    let query_point = DotProductPoint(query.to_vec());
                    for candidate in hnsw.search(&query_point, &mut search).take(k) {
                        // Keep negated distance for consistent internal sorting
                        // (lower = more similar). Un-negated at search() boundary.
                        results.push((*candidate.value as i64, candidate.distance));
                    }
                }
            }
        }

        results
    }

    /// Brute-force O(n) search over a specific set of vectors
    fn brute_force_search_vectors(
        &self,
        query: &[f32],
        k: usize,
        vectors: &[(u64, Vec<f32>)],
    ) -> Vec<(i64, f32)> {
        let mut results: Vec<(i64, f32)> = vectors
            .iter()
            .map(|(id, vec)| {
                let distance = self.compute_distance(query, vec);
                (*id as i64, distance)
            })
            .collect();

        // Sort by distance (ascending)
        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        results
    }

    /// VO-3: Brute-force O(n) search over quantized uint8 vectors using asymmetric distance.
    fn brute_force_search_quantized(
        &self,
        query: &[f32],
        k: usize,
        quantized_vectors: &[(u64, Vec<u8>)],
        quantizer: &ScalarQuantizer,
    ) -> Vec<(i64, f32)> {
        let mut results: Vec<(i64, f32)> = quantized_vectors
            .iter()
            .map(|(id, qvec)| {
                let distance = match self.config.metric {
                    DistanceMetric::Cosine => quantizer.asymmetric_cosine_distance(query, qvec),
                    DistanceMetric::Euclidean => quantizer.asymmetric_euclidean_distance(query, qvec),
                    DistanceMetric::DotProduct => quantizer.asymmetric_dot_distance(query, qvec),
                };
                (*id as i64, distance)
            })
            .collect();

        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        results
    }

    fn compute_distance(&self, a: &[f32], b: &[f32]) -> f32 {
        match self.config.metric {
            DistanceMetric::Euclidean => {
                a.iter()
                    .zip(b.iter())
                    .map(|(x, y)| (x - y).powi(2))
                    .sum::<f32>()
                    .sqrt()
            }
            DistanceMetric::Cosine => {
                let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                let norm_a: f32 = a.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
                let norm_b: f32 = b.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
                if norm_a == 0.0 || norm_b == 0.0 {
                    1.0
                } else {
                    1.0 - (dot / (norm_a * norm_b))
                }
            }
            DistanceMetric::DotProduct => {
                // Negate because we want to maximize dot product
                -a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>()
            }
        }
    }
}

/// Manages vector indexes for all topics and partitions
pub struct VectorIndexManager {
    /// Configuration
    config: VectorIndexConfig,
    /// Per-topic-partition indexes
    indexes: RwLock<HashMap<TopicPartitionKey, PartitionIndex>>,
    /// Topic configurations (dimensions, metric, etc.)
    topic_configs: RwLock<HashMap<String, HnswIndexConfig>>,
    /// Whether initial index loading from disk is complete (v2.3.1)
    loading_complete: AtomicBool,
}

impl VectorIndexManager {
    /// Create a new vector index manager
    pub fn new(config: VectorIndexConfig) -> Self {
        Self {
            config,
            indexes: RwLock::new(HashMap::new()),
            topic_configs: RwLock::new(HashMap::new()),
            loading_complete: AtomicBool::new(false),
        }
    }

    /// Register a topic with its HNSW configuration
    pub async fn register_topic(&self, topic: &str, config: HnswIndexConfig) {
        let mut configs = self.topic_configs.write().await;
        configs.insert(topic.to_string(), config);
        info!("Registered topic '{}' for vector search", topic);
    }

    /// Unregister a topic (does not delete existing indexes)
    pub async fn unregister_topic(&self, topic: &str) {
        let mut configs = self.topic_configs.write().await;
        configs.remove(topic);
        info!("Unregistered topic '{}' from vector search", topic);
    }

    /// Check if a topic is registered for vector search
    pub async fn is_topic_registered(&self, topic: &str) -> bool {
        let configs = self.topic_configs.read().await;
        configs.contains_key(topic)
    }

    /// Get or create an index for a topic-partition-model
    async fn get_or_create_index(&self, key: &TopicPartitionKey) -> Result<()> {
        // Check if already exists
        {
            let indexes = self.indexes.read().await;
            if indexes.contains_key(key) {
                return Ok(());
            }
        }

        // Get topic config or use default
        let config = {
            let configs = self.topic_configs.read().await;
            configs
                .get(&key.topic)
                .cloned()
                .unwrap_or_else(|| self.config.default_hnsw_config.clone())
        };

        // Create new index
        let mut indexes = self.indexes.write().await;
        if !indexes.contains_key(key) {
            // VO-5: Create index with the key's model_id
            indexes.insert(key.clone(), PartitionIndex::with_model_id(config, key.model_id.clone()));
            debug!(
                "Created vector index for topic={}, partition={}, model={}",
                key.topic, key.partition, key.model_id
            );
        }

        Ok(())
    }

    /// Add vectors to the index for a topic-partition
    pub async fn add_vectors(
        &self,
        topic: &str,
        partition: i32,
        entries: Vec<VectorEntry>,
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let key = TopicPartitionKey::new(topic, partition);
        self.get_or_create_index(&key).await?;

        let mut indexes = self.indexes.write().await;
        let index = indexes
            .get_mut(&key)
            .ok_or_else(|| anyhow!("Index not found after creation"))?;

        let mut added = 0;
        for entry in entries {
            if let Err(e) = index.add_vector(entry.offset, entry.vector) {
                warn!(
                    "Failed to add vector for offset {} to {}-{}: {}",
                    entry.offset, topic, partition, e
                );
            } else {
                added += 1;
            }
        }

        debug!(
            "Added {} vectors to index for {}-{} (total: {}, pending: {})",
            added,
            topic,
            partition,
            index.vector_count(),
            index.pending_vectors.len()
        );

        // VO-2: Automatically rebuild HNSW when pending buffer exceeds thresholds
        if index.needs_rebuild() {
            let pending = index.pending_vectors.len();
            let built = index.built_vectors.len();
            info!(
                "Auto-rebuilding HNSW for {}-{}: pending={}, built={}, total={}",
                topic, partition, pending, built, pending + built
            );
            if let Err(e) = index.build() {
                warn!(
                    "Failed to auto-rebuild HNSW for {}-{}: {}",
                    topic, partition, e
                );
            }
        }

        // VO-2: Report tier counts to metrics
        Self::report_tier_metrics(&indexes);

        Ok(added)
    }

    /// Search for k nearest neighbors in a topic-partition (default model)
    pub async fn search(
        &self,
        topic: &str,
        partition: i32,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(i64, f32)>> {
        self.search_model(topic, partition, query, k, DEFAULT_MODEL_ID).await
    }

    /// VO-5: Search for k nearest neighbors using a specific model's index
    pub async fn search_model(
        &self,
        topic: &str,
        partition: i32,
        query: &[f32],
        k: usize,
        model_id: &str,
    ) -> Result<Vec<(i64, f32)>> {
        let key = TopicPartitionKey::with_model(topic, partition, model_id);
        let indexes = self.indexes.read().await;

        let index = indexes
            .get(&key)
            .ok_or_else(|| anyhow!("No index for topic={}, partition={}, model={}", topic, partition, model_id))?;

        if query.len() != index.config.dimensions {
            return Err(anyhow!(
                "Query dimension mismatch: expected {}, got {}",
                index.config.dimensions,
                query.len()
            ));
        }

        Ok(index.search(query, k))
    }

    /// Search across all partitions of a topic (default model)
    pub async fn search_topic(
        &self,
        topic: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(i32, i64, f32)>> {
        self.search_topic_model(topic, query, k, DEFAULT_MODEL_ID).await
    }

    /// VO-5: Search across all partitions of a topic using a specific model's index
    pub async fn search_topic_model(
        &self,
        topic: &str,
        query: &[f32],
        k: usize,
        model_id: &str,
    ) -> Result<Vec<(i32, i64, f32)>> {
        let indexes = self.indexes.read().await;

        let mut all_results: Vec<(i32, i64, f32)> = Vec::new();

        for (key, index) in indexes.iter() {
            if key.topic == topic && key.model_id == model_id {
                let results = index.search(query, k);
                for (offset, distance) in results {
                    all_results.push((key.partition, offset, distance));
                }
            }
        }

        // Sort by distance and take top k
        all_results.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        all_results.truncate(k);

        Ok(all_results)
    }

    /// VO-5: Add vectors to a specific model's index
    pub async fn add_vectors_for_model(
        &self,
        topic: &str,
        partition: i32,
        model_id: &str,
        entries: Vec<VectorEntry>,
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let key = TopicPartitionKey::with_model(topic, partition, model_id);
        self.get_or_create_index(&key).await?;

        let mut indexes = self.indexes.write().await;
        let index = indexes
            .get_mut(&key)
            .ok_or_else(|| anyhow!("Index not found after creation"))?;

        let mut added = 0;
        for entry in entries {
            if let Err(e) = index.add_vector(entry.offset, entry.vector) {
                warn!(
                    "Failed to add vector for offset {} to {}-{} model={}: {}",
                    entry.offset, topic, partition, model_id, e
                );
            } else {
                added += 1;
            }
        }

        debug!(
            "Added {} vectors to index for {}-{} model={} (total: {}, pending: {})",
            added, topic, partition, model_id,
            index.vector_count(), index.pending_vectors.len()
        );

        // VO-2: Automatically rebuild HNSW when pending buffer exceeds thresholds
        if index.needs_rebuild() {
            let pending = index.pending_vectors.len();
            let built = index.built_vectors.len();
            info!(
                "Auto-rebuilding HNSW for {}-{} model={}: pending={}, built={}, total={}",
                topic, partition, model_id, pending, built, pending + built
            );
            if let Err(e) = index.build() {
                warn!(
                    "Failed to auto-rebuild HNSW for {}-{} model={}: {}",
                    topic, partition, model_id, e
                );
            }
        }

        Self::report_tier_metrics(&indexes);

        Ok(added)
    }

    /// VO-5: List all model IDs that have indexes for a given topic.
    pub async fn list_models(&self, topic: &str) -> Vec<String> {
        let indexes = self.indexes.read().await;
        let mut models: Vec<String> = indexes.keys()
            .filter(|k| k.topic == topic)
            .map(|k| k.model_id.clone())
            .collect();
        models.sort();
        models.dedup();
        models
    }

    /// VO-2/VO-3: Aggregate counts and memory across all partitions, report to metrics
    fn report_tier_metrics(indexes: &HashMap<TopicPartitionKey, PartitionIndex>) {
        let mut total_pending = 0usize;
        let mut total_built = 0usize;
        let mut total_memory = 0usize;
        for index in indexes.values() {
            total_pending += index.pending_vectors.len();
            total_built += index.built_vectors.len();
            total_memory += index.vector_memory_bytes();
        }
        MetricsRecorder::update_vector_index_counts(total_pending, total_built);
        MetricsRecorder::update_vector_memory_bytes(total_memory);
    }

    // ========================================================================
    // HNSW Index Building Methods
    // ========================================================================

    /// Build the HNSW index for a specific topic-partition
    ///
    /// This builds the O(log n) HNSW graph for the partition's vectors.
    /// Building is O(n log n) but enables faster O(log n) searches afterward.
    ///
    /// Returns true if the index was built, false if it was already built.
    pub async fn build_partition_index(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<bool> {
        let key = TopicPartitionKey::new(topic, partition);
        let mut indexes = self.indexes.write().await;

        let index = indexes
            .get_mut(&key)
            .ok_or_else(|| anyhow!("No index for topic={}, partition={}", topic, partition))?;

        // VO-2: Build if not built, or rebuild if pending vectors warrant it
        if index.is_built() && index.pending_vectors.is_empty() {
            return Ok(false);
        }

        index.build()?;
        info!(
            "Built HNSW index for {}-{} with {} vectors",
            topic, partition, index.vector_count()
        );
        Self::report_tier_metrics(&indexes);
        Ok(true)
    }

    /// Build HNSW indexes for all partitions of a topic
    ///
    /// Returns the number of indexes that were built.
    pub async fn build_topic_indexes(&self, topic: &str) -> Result<usize> {
        let keys: Vec<TopicPartitionKey> = {
            let indexes = self.indexes.read().await;
            indexes
                .keys()
                .filter(|k| k.topic == topic)
                .cloned()
                .collect()
        };

        let mut built = 0;
        for key in keys {
            let mut indexes = self.indexes.write().await;
            if let Some(index) = indexes.get_mut(&key) {
                // VO-2: Build if not built, or rebuild if pending vectors exist
                if !index.is_built() || !index.pending_vectors.is_empty() {
                    index.build()?;
                    built += 1;
                }
            }
        }

        if built > 0 {
            info!("Built {} HNSW indexes for topic '{}'", built, topic);
        }
        Ok(built)
    }

    /// Build all HNSW indexes that need building
    ///
    /// Returns the total number of indexes that were built.
    pub async fn build_all_indexes(&self) -> Result<usize> {
        let keys: Vec<TopicPartitionKey> = {
            let indexes = self.indexes.read().await;
            indexes.keys().cloned().collect()
        };

        let mut built = 0;
        for key in keys {
            let mut indexes = self.indexes.write().await;
            if let Some(index) = indexes.get_mut(&key) {
                // VO-2: Build if not built, or rebuild if pending vectors exist
                if !index.is_built() || !index.pending_vectors.is_empty() {
                    index.build()?;
                    built += 1;
                    debug!(
                        "Built HNSW index for {}-{} with {} vectors",
                        key.topic, key.partition, index.vector_count()
                    );
                }
            }
        }

        if built > 0 {
            info!("Built {} HNSW indexes across all topics", built);
            // VO-2: Report updated tier counts
            let indexes = self.indexes.read().await;
            Self::report_tier_metrics(&indexes);
        }
        Ok(built)
    }

    /// Get statistics for a topic-partition
    pub async fn get_partition_stats(
        &self,
        topic: &str,
        partition: i32,
    ) -> Option<PartitionIndexStats> {
        let key = TopicPartitionKey::new(topic, partition);
        let indexes = self.indexes.read().await;
        indexes.get(&key).map(|index| index.stats())
    }

    /// Get statistics for all partitions of a topic (aggregated across all models).
    ///
    /// VO-5: The `partitions` map is keyed by partition number. If multiple models
    /// exist for the same partition, the default model's stats are preferred.
    /// Use `get_topic_stats_for_model` for model-specific stats.
    pub async fn get_topic_stats(&self, topic: &str) -> TopicIndexStats {
        let indexes = self.indexes.read().await;

        let mut stats = TopicIndexStats::default();
        let mut models_seen = std::collections::HashSet::new();

        for (key, index) in indexes.iter() {
            if key.topic == topic {
                let partition_stats = index.stats();
                stats.total_vectors += partition_stats.vector_count;
                stats.dimensions = partition_stats.dimensions;
                models_seen.insert(key.model_id.clone());
                // Prefer default model's partition stats in backward-compat map
                if key.model_id == DEFAULT_MODEL_ID || !stats.partitions.contains_key(&key.partition) {
                    stats.partitions.insert(key.partition, partition_stats);
                }
            }
        }

        stats.partition_count = stats.partitions.len();
        stats.loading = !self.loading_complete.load(Ordering::Acquire);
        let mut models: Vec<String> = models_seen.into_iter().collect();
        models.sort();
        stats.models = models;
        stats
    }

    /// Get partition IDs that have >0 vectors locally for a given topic.
    pub async fn partitions_with_vectors(&self, topic: &str) -> Vec<i32> {
        let indexes = self.indexes.read().await;
        let mut partitions = std::collections::HashSet::new();
        for (key, index) in indexes.iter() {
            if key.topic == topic && index.vector_count() > 0 {
                partitions.insert(key.partition);
            }
        }
        let mut result: Vec<i32> = partitions.into_iter().collect();
        result.sort();
        result
    }

    /// VO-5: Get statistics for a specific model's indexes within a topic.
    pub async fn get_topic_stats_for_model(&self, topic: &str, model_id: &str) -> TopicIndexStats {
        let indexes = self.indexes.read().await;

        let mut stats = TopicIndexStats::default();

        for (key, index) in indexes.iter() {
            if key.topic == topic && key.model_id == model_id {
                let partition_stats = index.stats();
                stats.total_vectors += partition_stats.vector_count;
                stats.dimensions = partition_stats.dimensions;
                stats.partitions.insert(key.partition, partition_stats);
            }
        }

        stats.partition_count = stats.partitions.len();
        stats.loading = !self.loading_complete.load(Ordering::Acquire);
        stats.models = if stats.partition_count > 0 { vec![model_id.to_string()] } else { vec![] };
        stats
    }

    /// Get list of topics with vector indexes
    pub async fn list_topics(&self) -> Vec<String> {
        let indexes = self.indexes.read().await;
        let mut topics: Vec<String> = indexes.keys().map(|k| k.topic.clone()).collect();
        topics.sort();
        topics.dedup();
        topics
    }

    /// Clear index for a topic-partition
    pub async fn clear_partition(&self, topic: &str, partition: i32) {
        let key = TopicPartitionKey::new(topic, partition);
        let mut indexes = self.indexes.write().await;
        indexes.remove(&key);
        debug!("Cleared vector index for {}-{}", topic, partition);
    }

    /// Clear all indexes for a topic
    pub async fn clear_topic(&self, topic: &str) {
        let mut indexes = self.indexes.write().await;
        indexes.retain(|k, _| k.topic != topic);
        debug!("Cleared all vector indexes for topic '{}'", topic);
    }

    // ========================================================================
    // Persistence Methods (Phase 5.3 + VO-5)
    // ========================================================================

    /// VO-5: Get the snapshot filename for a partition index.
    ///
    /// - Default model: `{partition}.idx` (backward compatible)
    /// - Named model: `{partition}_{model_id}.idx`
    fn snapshot_filename(partition: i32, model_id: &str) -> String {
        if model_id == DEFAULT_MODEL_ID {
            format!("{}.idx", partition)
        } else {
            format!("{}_{}.idx", partition, model_id)
        }
    }

    /// VO-5: Parse a snapshot filename into (partition, model_id).
    ///
    /// - `"0.idx"` → `Some((0, "default"))`
    /// - `"0_text-embedding-3-large.idx"` → `Some((0, "text-embedding-3-large"))`
    /// - `"invalid"` → `None`
    fn parse_snapshot_filename(filename: &str) -> Option<(i32, String)> {
        let stem = filename.strip_suffix(".idx")?;
        if let Some(sep_pos) = stem.find('_') {
            let partition: i32 = stem[..sep_pos].parse().ok()?;
            let model_id = stem[sep_pos + 1..].to_string();
            if model_id.is_empty() {
                return None;
            }
            Some((partition, model_id))
        } else {
            let partition: i32 = stem.parse().ok()?;
            Some((partition, DEFAULT_MODEL_ID.to_string()))
        }
    }

    /// Save a single partition index to disk (default model)
    ///
    /// Serializes the index using bincode and writes to:
    /// `{base_path}/{topic}/{partition}.idx`
    pub async fn save_partition_index(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<()> {
        self.save_partition_index_for_model(topic, partition, DEFAULT_MODEL_ID).await
    }

    /// VO-5: Save a partition index for a specific model to disk.
    ///
    /// Writes to: `{base_path}/{topic}/{partition}_{model_id}.idx`
    /// (or `{partition}.idx` for the default model)
    pub async fn save_partition_index_for_model(
        &self,
        topic: &str,
        partition: i32,
        model_id: &str,
    ) -> Result<()> {
        let key = TopicPartitionKey::with_model(topic, partition, model_id);

        // Get the index data
        let snapshot = {
            let indexes = self.indexes.read().await;
            let index = indexes
                .get(&key)
                .ok_or_else(|| anyhow!("No index for {}-{} model={}", topic, partition, model_id))?;

            PartitionIndexSnapshot {
                config: index.config.clone(),
                // VO-2: Combine both tiers into a single snapshot for backward compat
                vectors: index.built_vectors.iter()
                    .chain(index.pending_vectors.iter())
                    .cloned()
                    .collect(),
                last_offset: index.last_offset,
                // VO-3: Include trained quantizer for uint8 mode
                quantizer: index.quantizer.clone(),
                // VO-5: Include model ID
                model_id: index.model_id.clone(),
            }
        };

        // Create directory structure
        let topic_dir = self.config.base_path.join(topic);
        fs::create_dir_all(&topic_dir).await?;

        // Serialize and write
        let filename = Self::snapshot_filename(partition, model_id);
        let path = topic_dir.join(&filename);
        let data = bincode::serialize(&snapshot)
            .map_err(|e| anyhow!("Failed to serialize index: {}", e))?;

        fs::write(&path, &data).await?;
        info!(
            topic = topic,
            partition = partition,
            model_id = model_id,
            vectors = snapshot.vectors.len(),
            path = %path.display(),
            "Saved partition index"
        );

        Ok(())
    }

    /// Load a single partition index from disk (default model)
    ///
    /// Loads from `{base_path}/{topic}/{partition}.idx`
    pub async fn load_partition_index(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<()> {
        self.load_partition_index_for_model(topic, partition, DEFAULT_MODEL_ID).await
    }

    /// VO-5: Load a partition index for a specific model from disk.
    pub async fn load_partition_index_for_model(
        &self,
        topic: &str,
        partition: i32,
        model_id: &str,
    ) -> Result<()> {
        let filename = Self::snapshot_filename(partition, model_id);
        let path = self.config.base_path.join(topic).join(&filename);

        if !path.exists() {
            return Err(anyhow!("Index file not found: {}", path.display()));
        }

        let data = fs::read(&path).await?;
        let snapshot: PartitionIndexSnapshot = bincode::deserialize(&data)
            .map_err(|e| anyhow!("Failed to deserialize index: {}", e))?;

        // VO-5: Use model_id from snapshot (defaults to "default" for old snapshots)
        let effective_model = if snapshot.model_id.is_empty() {
            model_id.to_string()
        } else {
            snapshot.model_id.clone()
        };

        let key = TopicPartitionKey::with_model(topic, partition, &effective_model);
        let vector_count = snapshot.vectors.len();
        let mut index = PartitionIndex::with_model_id(snapshot.config, effective_model.clone());
        // VO-2: Load into built_vectors (pending starts empty). build() below
        // will construct the HNSW graph over them.
        index.built_vectors = snapshot.vectors;
        index.last_offset = snapshot.last_offset;
        // VO-3: Restore quantizer from snapshot (if present)
        index.quantizer = snapshot.quantizer;

        // Rebuild the HNSW index after loading vectors (also trains quantizer for uint8)
        if vector_count > 0 {
            if let Err(e) = index.build() {
                warn!(
                    topic = topic,
                    partition = partition,
                    model_id = %effective_model,
                    error = %e,
                    "Failed to rebuild HNSW index after load"
                );
            }
        }

        // Register the topic config if not already registered
        if !self.is_topic_registered(topic).await {
            self.register_topic(topic, index.config.clone()).await;
        }

        // Track is_built before moving index
        let is_built = index.is_built();

        // Store the index
        let mut indexes = self.indexes.write().await;
        indexes.insert(key, index);

        info!(
            topic = topic,
            partition = partition,
            model_id = %effective_model,
            vectors = vector_count,
            is_built = is_built,
            path = %path.display(),
            "Loaded partition index"
        );

        Ok(())
    }

    /// Save all indexes to disk
    ///
    /// VO-5: Iterates through all indexes (all models) and saves each one.
    pub async fn save_all_indexes(&self) -> Result<usize> {
        let keys: Vec<TopicPartitionKey> = {
            let indexes = self.indexes.read().await;
            indexes.keys().cloned().collect()
        };

        let mut saved = 0;
        for key in keys {
            match self.save_partition_index_for_model(&key.topic, key.partition, &key.model_id).await {
                Ok(_) => saved += 1,
                Err(e) => {
                    error!(
                        topic = %key.topic,
                        partition = key.partition,
                        model_id = %key.model_id,
                        error = %e,
                        "Failed to save partition index"
                    );
                }
            }
        }

        info!(saved = saved, "Saved all indexes");
        Ok(saved)
    }

    /// Load all indexes from disk
    ///
    /// VO-5: Scans the base path for index files and loads each one.
    /// Supports both legacy (`{partition}.idx`) and model-tagged (`{partition}_{model_id}.idx`) files.
    pub async fn load_all_indexes(&self) -> Result<usize> {
        let base_path = &self.config.base_path;

        if !base_path.exists() {
            info!("Vector index directory does not exist, starting fresh");
            return Ok(0);
        }

        let mut loaded = 0;
        let mut entries = fs::read_dir(base_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let topic = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            // Load all partition indexes for this topic
            let mut partition_entries = fs::read_dir(&path).await?;
            while let Some(partition_entry) = partition_entries.next_entry().await? {
                let partition_path = partition_entry.path();
                let file_name = match partition_path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                // VO-5: Parse filename to extract partition and model_id
                if !file_name.ends_with(".idx") {
                    continue;
                }
                let (partition, model_id) = match Self::parse_snapshot_filename(&file_name) {
                    Some(parsed) => parsed,
                    None => continue,
                };

                match self.load_partition_index_for_model(&topic, partition, &model_id).await {
                    Ok(_) => loaded += 1,
                    Err(e) => {
                        warn!(
                            topic = %topic,
                            partition = partition,
                            model_id = %model_id,
                            error = %e,
                            "Failed to load partition index"
                        );
                    }
                }
            }
        }

        self.loading_complete.store(true, Ordering::Release);
        info!(loaded = loaded, "Loaded all indexes from disk");
        Ok(loaded)
    }

    /// Start a background task that periodically snapshots indexes
    ///
    /// # Arguments
    /// * `interval_secs` - Seconds between snapshots (0 to disable)
    ///
    /// # Returns
    /// A JoinHandle for the background task
    pub fn start_snapshot_task(
        self: &Arc<Self>,
        interval_secs: u64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if interval_secs == 0 {
            info!("Vector index snapshotting disabled (interval=0)");
            return None;
        }

        let manager = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut timer = interval(Duration::from_secs(interval_secs));

            loop {
                timer.tick().await;

                debug!("Running periodic vector index snapshot");
                match manager.save_all_indexes().await {
                    Ok(count) => {
                        if count > 0 {
                            debug!(count = count, "Periodic snapshot completed");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Periodic snapshot failed");
                    }
                }
            }
        });

        info!(
            interval_secs = interval_secs,
            "Started vector index snapshot task"
        );
        Some(handle)
    }

    /// Get the path where an index would be stored (default model)
    pub fn index_path(&self, topic: &str, partition: i32) -> PathBuf {
        self.index_path_for_model(topic, partition, DEFAULT_MODEL_ID)
    }

    /// VO-5: Get the path where a model-specific index would be stored
    pub fn index_path_for_model(&self, topic: &str, partition: i32, model_id: &str) -> PathBuf {
        let filename = Self::snapshot_filename(partition, model_id);
        self.config.base_path.join(topic).join(filename)
    }
}

/// Serializable snapshot of a partition index.
///
/// VO-3: Backward-compatible. Old snapshots (no quantizer field) deserialize with
/// `quantizer = None`, which means f32 mode. The `config.quantization` field has
/// `#[serde(default)]` so it defaults to F32 for old snapshots.
///
/// VO-5: Added `model_id`. Old snapshots without it default to `"default"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionIndexSnapshot {
    /// HNSW configuration
    pub config: HnswIndexConfig,
    /// All vectors (id, vector) — always stored as f32 for HNSW build and full-precision fallback
    pub vectors: Vec<(u64, Vec<f32>)>,
    /// Last offset indexed
    pub last_offset: Option<i64>,
    /// VO-3: Trained scalar quantizer (for uint8 mode)
    #[serde(default)]
    pub quantizer: Option<ScalarQuantizer>,
    /// VO-5: Embedding model ID that produced these vectors
    #[serde(default = "default_model_id")]
    pub model_id: String,
}

fn default_model_id() -> String {
    "default".to_string()
}

/// Embedding pipeline for batch processing messages
pub struct EmbeddingPipeline {
    /// Embedding provider
    provider: Arc<dyn EmbeddingProvider>,
    /// Vector index manager
    index_manager: Arc<VectorIndexManager>,
    /// Batch size for embedding requests (clamped to provider max)
    batch_size: usize,
    /// Maximum concurrent in-flight embedding API calls
    concurrency: usize,
}

impl EmbeddingPipeline {
    /// Create a new embedding pipeline
    pub fn new(
        provider: Arc<dyn EmbeddingProvider>,
        index_manager: Arc<VectorIndexManager>,
        batch_size: usize,
    ) -> Self {
        // Clamp batch_size to provider's limit
        let provider_max = provider.max_batch_size();
        let effective_batch_size = batch_size.max(1).min(provider_max);
        Self {
            provider,
            index_manager,
            batch_size: effective_batch_size,
            concurrency: 4,
        }
    }

    /// Create a new embedding pipeline with custom concurrency
    pub fn with_concurrency(
        provider: Arc<dyn EmbeddingProvider>,
        index_manager: Arc<VectorIndexManager>,
        batch_size: usize,
        concurrency: usize,
    ) -> Self {
        let provider_max = provider.max_batch_size();
        let effective_batch_size = batch_size.max(1).min(provider_max);
        let effective_concurrency = concurrency.max(1).min(32);
        // VO-1: Report pipeline config to metrics
        MetricsRecorder::update_embedding_pipeline_config(effective_batch_size, effective_concurrency);
        Self {
            provider,
            index_manager,
            batch_size: effective_batch_size,
            concurrency: effective_concurrency,
        }
    }

    /// Process a batch of messages and add embeddings to the index (default model).
    pub async fn process_messages(
        &self,
        topic: &str,
        partition: i32,
        messages: Vec<(i64, String)>,
    ) -> Result<ProcessingStats> {
        self.process_messages_for_model(topic, partition, messages, None).await
    }

    /// Process a batch of messages and add embeddings to a specific model's index.
    ///
    /// When `model_id` is Some, vectors are stored under that model's index via
    /// `add_vectors_for_model`. When None, uses the default model via `add_vectors`.
    ///
    /// Batches are sent to the embedding provider concurrently (up to `self.concurrency`
    /// in-flight at once), then results are added to the index in offset order.
    pub async fn process_messages_for_model(
        &self,
        topic: &str,
        partition: i32,
        messages: Vec<(i64, String)>, // (offset, text)
        model_id: Option<&str>,
    ) -> Result<ProcessingStats> {
        if messages.is_empty() {
            return Ok(ProcessingStats::default());
        }

        let mut stats = ProcessingStats {
            total_messages: messages.len(),
            ..Default::default()
        };

        // Prepare all chunks upfront with their batch index for ordered collection
        let chunks: Vec<(usize, Vec<(i64, String)>)> = messages
            .chunks(self.batch_size)
            .enumerate()
            .map(|(i, chunk)| (i, chunk.to_vec()))
            .collect();

        let total_batches = chunks.len();

        if self.concurrency <= 1 || total_batches <= 1 {
            // Serial path: single batch or concurrency=1
            for (_batch_idx, chunk) in chunks {
                self.process_single_batch(topic, partition, &chunk, &mut stats, model_id).await;
            }
        } else {
            // Concurrent path: process up to `concurrency` batches in parallel
            let semaphore = Arc::new(tokio::sync::Semaphore::new(self.concurrency));
            let mut join_set = tokio::task::JoinSet::new();

            for (batch_idx, chunk) in chunks {
                let permit = semaphore.clone().acquire_owned().await
                    .map_err(|e| anyhow!("Semaphore closed: {}", e))?;
                let provider = Arc::clone(&self.provider);
                let topic_owned = topic.to_string();

                join_set.spawn(async move {
                    let texts: Vec<&str> = chunk.iter().map(|(_, text)| text.as_str()).collect();
                    let offsets: Vec<i64> = chunk.iter().map(|(offset, _)| *offset).collect();
                    let chunk_len = chunk.len();

                    MetricsRecorder::increment_embedding_queue();
                    let embed_start = Instant::now();

                    let result = provider.embed_batch(&texts).await;

                    let latency = embed_start.elapsed();
                    MetricsRecorder::record_embedding_latency(latency);
                    MetricsRecorder::decrement_embedding_queue();

                    drop(permit); // Release semaphore slot

                    match result {
                        Ok(batch) => {
                            let entries: Vec<VectorEntry> = batch
                                .embeddings
                                .into_iter()
                                .zip(offsets.iter())
                                .map(|(emb_result, &offset)| VectorEntry {
                                    offset,
                                    vector: emb_result.vector,
                                    text_preview: Some(truncate_to_char_boundary(&emb_result.text, 100).to_string()),
                                })
                                .collect();
                            let tokens_used = batch.total_tokens.unwrap_or(0);

                            MetricsRecorder::record_embedding_request(
                                true,
                                chunk_len as u64,
                                entries.len() as u64,
                                tokens_used as u64,
                            );

                            (batch_idx, Ok((entries, tokens_used, topic_owned)))
                        }
                        Err(e) => {
                            warn!(
                                "Embedding batch {} failed for {}: {}",
                                batch_idx, topic_owned, e
                            );
                            MetricsRecorder::record_embedding_request(
                                false,
                                chunk_len as u64,
                                0,
                                0,
                            );
                            (batch_idx, Err((chunk_len, e.to_string())))
                        }
                    }
                });
            }

            // Collect results and add to index in batch order
            let mut results: Vec<(usize, std::result::Result<(Vec<VectorEntry>, usize, String), (usize, String)>)> = Vec::with_capacity(total_batches);
            while let Some(join_result) = join_set.join_next().await {
                match join_result {
                    Ok(batch_result) => results.push(batch_result),
                    Err(e) => {
                        warn!("Embedding task panicked: {}", e);
                    }
                }
            }

            // Sort by batch index for deterministic offset-order insertion
            results.sort_by_key(|(idx, _)| *idx);

            for (_batch_idx, result) in results {
                match result {
                    Ok((entries, tokens_used, _topic)) => {
                        let added = if let Some(mid) = model_id {
                            self.index_manager
                                .add_vectors_for_model(topic, partition, mid, entries)
                                .await?
                        } else {
                            self.index_manager
                                .add_vectors(topic, partition, entries)
                                .await?
                        };

                        stats.embedded += added;
                        stats.batches_processed += 1;
                        if tokens_used > 0 {
                            stats.total_tokens += tokens_used;
                        }
                    }
                    Err((chunk_len, error_msg)) => {
                        stats.failed += chunk_len;
                        stats.errors.push(error_msg);
                    }
                }
            }
        }

        info!(
            "Processed {} messages for {}-{}: {} embedded, {} failed ({} batches, concurrency={})",
            stats.total_messages, topic, partition, stats.embedded, stats.failed,
            stats.batches_processed, self.concurrency
        );

        Ok(stats)
    }

    /// Process a single batch of messages serially (used for concurrency=1 path)
    async fn process_single_batch(
        &self,
        topic: &str,
        partition: i32,
        chunk: &[(i64, String)],
        stats: &mut ProcessingStats,
        model_id: Option<&str>,
    ) {
        let texts: Vec<&str> = chunk.iter().map(|(_, text)| text.as_str()).collect();
        let offsets: Vec<i64> = chunk.iter().map(|(offset, _)| *offset).collect();

        MetricsRecorder::increment_embedding_queue();
        let embed_start = Instant::now();

        match self.provider.embed_batch(&texts).await {
            Ok(batch) => {
                let latency = embed_start.elapsed();
                MetricsRecorder::record_embedding_latency(latency);
                MetricsRecorder::decrement_embedding_queue();

                let entries: Vec<VectorEntry> = batch
                    .embeddings
                    .into_iter()
                    .zip(offsets.iter())
                    .map(|(result, &offset)| VectorEntry {
                        offset,
                        vector: result.vector,
                        text_preview: Some(truncate_to_char_boundary(&result.text, 100).to_string()),
                    })
                    .collect();

                let add_result = if let Some(mid) = model_id {
                    self.index_manager.add_vectors_for_model(topic, partition, mid, entries).await
                } else {
                    self.index_manager.add_vectors(topic, partition, entries).await
                };
                match add_result {
                    Ok(added) => {
                        stats.embedded += added;
                        stats.batches_processed += 1;
                        let tokens_used = batch.total_tokens.unwrap_or(0);
                        if tokens_used > 0 {
                            stats.total_tokens += tokens_used;
                        }
                        MetricsRecorder::record_embedding_request(
                            true,
                            chunk.len() as u64,
                            added as u64,
                            tokens_used as u64,
                        );
                    }
                    Err(e) => {
                        warn!("Failed to add vectors for {}-{}: {}", topic, partition, e);
                        stats.failed += chunk.len();
                        stats.errors.push(e.to_string());
                    }
                }
            }
            Err(e) => {
                let latency = embed_start.elapsed();
                MetricsRecorder::record_embedding_latency(latency);
                MetricsRecorder::decrement_embedding_queue();

                warn!(
                    "Embedding batch failed for {}-{}: {}",
                    topic, partition, e
                );
                stats.failed += chunk.len();
                stats.errors.push(e.to_string());

                MetricsRecorder::record_embedding_request(
                    false,
                    chunk.len() as u64,
                    0,
                    0,
                );
            }
        }
    }
}

/// Statistics from embedding processing
#[derive(Debug, Clone, Default)]
pub struct ProcessingStats {
    /// Total messages processed
    pub total_messages: usize,
    /// Successfully embedded
    pub embedded: usize,
    /// Failed to embed
    pub failed: usize,
    /// Number of batches processed
    pub batches_processed: usize,
    /// Total tokens used (if available)
    pub total_tokens: usize,
    /// Error messages
    pub errors: Vec<String>,
}

// ============================================================================
// Vector Search Service (Phase 5.4)
// ============================================================================

/// Result from a vector search query
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    /// Topic name
    pub topic: String,
    /// Partition number
    pub partition: i32,
    /// Message offset
    pub offset: i64,
    /// Distance/similarity score (lower = more similar for distance metrics)
    pub score: f32,
    /// Original text preview (if available)
    pub text_preview: Option<String>,
    /// Embedding vector (optional, included when include_embeddings=true)
    pub embedding: Option<Vec<f32>>,
}

/// Filters for vector search queries
#[derive(Debug, Clone, Default)]
pub struct VectorSearchFilters {
    /// Filter to specific partitions (None = all partitions)
    pub partitions: Option<Vec<i32>>,
    /// Minimum offset (inclusive)
    pub min_offset: Option<i64>,
    /// Maximum offset (inclusive)
    pub max_offset: Option<i64>,
    /// Minimum timestamp (inclusive, milliseconds)
    pub min_timestamp: Option<i64>,
    /// Maximum timestamp (inclusive, milliseconds)
    pub max_timestamp: Option<i64>,
}

impl VectorSearchFilters {
    /// Create empty filters (match all)
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter to specific partitions
    pub fn with_partitions(mut self, partitions: Vec<i32>) -> Self {
        self.partitions = Some(partitions);
        self
    }

    /// Filter to single partition
    pub fn with_partition(mut self, partition: i32) -> Self {
        self.partitions = Some(vec![partition]);
        self
    }

    /// Filter by offset range
    pub fn with_offset_range(mut self, min: i64, max: i64) -> Self {
        self.min_offset = Some(min);
        self.max_offset = Some(max);
        self
    }

    /// Filter by minimum offset
    pub fn with_min_offset(mut self, min: i64) -> Self {
        self.min_offset = Some(min);
        self
    }

    /// Filter by maximum offset
    pub fn with_max_offset(mut self, max: i64) -> Self {
        self.max_offset = Some(max);
        self
    }

    /// Check if an offset passes the filter
    pub fn matches_offset(&self, offset: i64) -> bool {
        if let Some(min) = self.min_offset {
            if offset < min {
                return false;
            }
        }
        if let Some(max) = self.max_offset {
            if offset > max {
                return false;
            }
        }
        true
    }

    /// Check if a partition passes the filter
    pub fn matches_partition(&self, partition: i32) -> bool {
        match &self.partitions {
            Some(partitions) => partitions.contains(&partition),
            None => true,
        }
    }
}

/// Vector search service providing text-based and filtered search
///
/// Wraps VectorIndexManager with:
/// - Text query support (embed query text, then search)
/// - Filter support (partition, offset range)
/// - Rich result types with metadata
pub struct VectorSearchService {
    /// Vector index manager
    index_manager: Arc<VectorIndexManager>,
    /// Embedding provider for query embedding (None for index-only/raw-vector searches)
    provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Optional query embedding cache (SV-1: reduces repeated embedding API calls)
    embedding_cache: Option<Arc<crate::vector_cache::QueryEmbeddingCache>>,
}

impl VectorSearchService {
    /// Create a new vector search service
    pub fn new(
        index_manager: Arc<VectorIndexManager>,
        provider: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            index_manager,
            provider: Some(provider),
            embedding_cache: None,
        }
    }

    /// Create a search service for raw-vector searches only (no embedding provider needed).
    ///
    /// This supports `search_by_vector`, `search_by_vector_model`, `get_vector_count`,
    /// and other methods that don't require query-time embedding.
    pub fn new_index_only(index_manager: Arc<VectorIndexManager>) -> Self {
        Self {
            index_manager,
            provider: None,
            embedding_cache: None,
        }
    }

    /// Attach a query embedding cache (builder pattern).
    pub fn with_cache(mut self, cache: Option<Arc<crate::vector_cache::QueryEmbeddingCache>>) -> Self {
        self.embedding_cache = cache;
        self
    }

    /// Search by text query
    ///
    /// Embeds the query text using the configured provider, then searches
    /// the vector index for the k most similar messages.
    /// If a query embedding cache is configured, checks the cache first
    /// to avoid repeated embedding API calls.
    ///
    /// # Arguments
    /// * `topic` - Topic to search
    /// * `query_text` - Text to search for (will be embedded)
    /// * `k` - Number of results to return
    /// * `filters` - Optional filters (partition, offset range)
    ///
    /// # Example
    /// ```ignore
    /// let results = service.search_by_text("logs", "connection error", 10, None).await?;
    /// for result in results {
    ///     println!("Offset {} (score: {})", result.offset, result.score);
    /// }
    /// ```
    pub async fn search_by_text(
        &self,
        topic: &str,
        query_text: &str,
        k: usize,
        filters: Option<VectorSearchFilters>,
    ) -> Result<Vec<VectorSearchResult>> {
        let cache_key = query_text.trim().to_lowercase();

        // Check embedding cache first
        if let Some(ref cache) = self.embedding_cache {
            if let Some(cached_vector) = cache.get(&cache_key).await {
                chronik_monitoring::MetricsRecorder::record_embedding_cache_operation("hit");
                return self.search_by_vector(topic, &cached_vector, k, filters).await;
            }
            chronik_monitoring::MetricsRecorder::record_embedding_cache_operation("miss");
        }

        // Cache miss — call embedding provider
        let provider = self.provider.as_ref()
            .ok_or_else(|| anyhow!("Embedding provider not configured (use search_by_vector for raw vectors)"))?;
        let embedding_result = provider.embed(query_text).await?;
        let query_vector = embedding_result.vector;

        // Store in cache for future queries
        if let Some(ref cache) = self.embedding_cache {
            cache.insert(cache_key, query_vector.clone()).await;
            chronik_monitoring::MetricsRecorder::update_embedding_cache_size(
                cache.size().await,
                cache.memory_bytes().await,
            );
        }

        // Search by vector
        self.search_by_vector(topic, &query_vector, k, filters).await
    }

    /// Search by vector directly
    ///
    /// Searches the vector index for the k most similar messages to the
    /// provided query vector.
    ///
    /// # Arguments
    /// * `topic` - Topic to search
    /// * `query_vector` - Query embedding vector
    /// * `k` - Number of results to return
    /// * `filters` - Optional filters (partition, offset range)
    pub async fn search_by_vector(
        &self,
        topic: &str,
        query_vector: &[f32],
        k: usize,
        filters: Option<VectorSearchFilters>,
    ) -> Result<Vec<VectorSearchResult>> {
        let filters = filters.unwrap_or_default();

        // Get matching partitions
        let stats = self.index_manager.get_topic_stats(topic).await;
        if stats.partition_count == 0 {
            return Ok(Vec::new());
        }

        let partitions_to_search: Vec<i32> = stats
            .partitions
            .keys()
            .filter(|&&p| filters.matches_partition(p))
            .copied()
            .collect();

        if partitions_to_search.is_empty() {
            return Ok(Vec::new());
        }

        // Search each partition and collect results
        // Request more results than k to account for filtering
        let results_per_partition = k * 2;
        let mut all_results: Vec<VectorSearchResult> = Vec::new();

        for partition in partitions_to_search {
            match self
                .index_manager
                .search(topic, partition, query_vector, results_per_partition)
                .await
            {
                Ok(partition_results) => {
                    for (offset, score) in partition_results {
                        // Apply offset filter
                        if !filters.matches_offset(offset) {
                            continue;
                        }

                        all_results.push(VectorSearchResult {
                            topic: topic.to_string(),
                            partition,
                            offset,
                            score,
                            text_preview: None, // TODO: Could fetch from index if stored
                            embedding: None, // TODO: Include when include_embeddings=true
                        });
                    }
                }
                Err(e) => {
                    // Log error but continue with other partitions
                    warn!(
                        topic = topic,
                        partition = partition,
                        error = %e,
                        "Failed to search partition"
                    );
                }
            }
        }

        // Sort by score (ascending for distance metrics) and take top k
        all_results.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all_results.truncate(k);

        Ok(all_results)
    }

    /// Get the number of indexed vectors for a topic
    pub async fn get_vector_count(&self, topic: &str) -> usize {
        self.index_manager.get_topic_stats(topic).await.total_vectors
    }

    /// Check if a topic has any indexed vectors
    pub async fn has_vectors(&self, topic: &str) -> bool {
        self.get_vector_count(topic).await > 0
    }

    /// Get the embedding provider name (returns "none" if no provider configured)
    pub fn provider_name(&self) -> &str {
        self.provider.as_ref().map(|p| p.name()).unwrap_or("none")
    }

    /// Get the embedding dimensions (returns 0 if no provider configured)
    pub fn dimensions(&self) -> usize {
        self.provider.as_ref().map(|p| p.dimensions()).unwrap_or(0)
    }

    /// VO-5: Search by text query against a specific model's index.
    pub async fn search_by_text_model(
        &self,
        topic: &str,
        query_text: &str,
        k: usize,
        filters: Option<VectorSearchFilters>,
        model_id: &str,
    ) -> Result<Vec<VectorSearchResult>> {
        let cache_key = format!("{}:{}", model_id, query_text.trim().to_lowercase());

        // Check embedding cache first
        if let Some(ref cache) = self.embedding_cache {
            if let Some(cached_vector) = cache.get(&cache_key).await {
                chronik_monitoring::MetricsRecorder::record_embedding_cache_operation("hit");
                return self.search_by_vector_model(topic, &cached_vector, k, filters, model_id).await;
            }
            chronik_monitoring::MetricsRecorder::record_embedding_cache_operation("miss");
        }

        // Cache miss — call embedding provider
        let provider = self.provider.as_ref()
            .ok_or_else(|| anyhow!("Embedding provider not configured (use search_by_vector_model for raw vectors)"))?;
        let embedding_result = provider.embed(query_text).await?;
        let query_vector = embedding_result.vector;

        // Store in cache for future queries
        if let Some(ref cache) = self.embedding_cache {
            cache.insert(cache_key, query_vector.clone()).await;
            chronik_monitoring::MetricsRecorder::update_embedding_cache_size(
                cache.size().await,
                cache.memory_bytes().await,
            );
        }

        self.search_by_vector_model(topic, &query_vector, k, filters, model_id).await
    }

    /// VO-5: Search by vector against a specific model's index.
    pub async fn search_by_vector_model(
        &self,
        topic: &str,
        query_vector: &[f32],
        k: usize,
        filters: Option<VectorSearchFilters>,
        model_id: &str,
    ) -> Result<Vec<VectorSearchResult>> {
        let filters = filters.unwrap_or_default();

        // Get stats for the specific model
        let stats = self.index_manager.get_topic_stats_for_model(topic, model_id).await;
        if stats.partition_count == 0 {
            return Ok(Vec::new());
        }

        let partitions_to_search: Vec<i32> = stats
            .partitions
            .keys()
            .filter(|&&p| filters.matches_partition(p))
            .copied()
            .collect();

        if partitions_to_search.is_empty() {
            return Ok(Vec::new());
        }

        let results_per_partition = k * 2;
        let mut all_results: Vec<VectorSearchResult> = Vec::new();

        for partition in partitions_to_search {
            match self
                .index_manager
                .search_model(topic, partition, query_vector, results_per_partition, model_id)
                .await
            {
                Ok(partition_results) => {
                    for (offset, score) in partition_results {
                        if !filters.matches_offset(offset) {
                            continue;
                        }
                        all_results.push(VectorSearchResult {
                            topic: topic.to_string(),
                            partition,
                            offset,
                            score,
                            text_preview: None,
                            embedding: None,
                        });
                    }
                }
                Err(e) => {
                    warn!(
                        topic = topic,
                        partition = partition,
                        model_id = model_id,
                        error = %e,
                        "Failed to search partition for model"
                    );
                }
            }
        }

        all_results.sort_by(|a, b| {
            a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        all_results.truncate(k);

        Ok(all_results)
    }

    /// VO-5: Get vector count for a specific model's index.
    pub async fn get_vector_count_for_model(&self, topic: &str, model_id: &str) -> usize {
        self.index_manager.get_topic_stats_for_model(topic, model_id).await.total_vectors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_vector_index_manager_basic() {
        let config = VectorIndexConfig::default();
        let manager = VectorIndexManager::new(config);

        // Register topic
        let hnsw_config = HnswIndexConfig {
            dimensions: 3,
            ..Default::default()
        };
        manager.register_topic("test-topic", hnsw_config).await;

        // Add vectors
        let entries = vec![
            VectorEntry {
                offset: 0,
                vector: vec![1.0, 0.0, 0.0],
                text_preview: Some("hello".to_string()),
            },
            VectorEntry {
                offset: 1,
                vector: vec![0.0, 1.0, 0.0],
                text_preview: Some("world".to_string()),
            },
        ];
        let added = manager.add_vectors("test-topic", 0, entries).await.unwrap();
        assert_eq!(added, 2);

        // Search
        let results = manager
            .search("test-topic", 0, &[0.9, 0.1, 0.0], 1)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0); // Closest to first vector
    }

    #[tokio::test]
    async fn test_topic_registration() {
        let manager = VectorIndexManager::new(VectorIndexConfig::default());

        assert!(!manager.is_topic_registered("test").await);

        manager
            .register_topic("test", HnswIndexConfig::default())
            .await;
        assert!(manager.is_topic_registered("test").await);

        manager.unregister_topic("test").await;
        assert!(!manager.is_topic_registered("test").await);
    }

    #[tokio::test]
    async fn test_multi_partition_search() {
        let manager = VectorIndexManager::new(VectorIndexConfig::default());

        let config = HnswIndexConfig {
            dimensions: 2,
            ..Default::default()
        };
        manager.register_topic("test", config).await;

        // Add vectors to partition 0
        manager
            .add_vectors(
                "test",
                0,
                vec![VectorEntry {
                    offset: 0,
                    vector: vec![1.0, 0.0],
                    text_preview: None,
                }],
            )
            .await
            .unwrap();

        // Add vectors to partition 1
        manager
            .add_vectors(
                "test",
                1,
                vec![VectorEntry {
                    offset: 10,
                    vector: vec![0.0, 1.0],
                    text_preview: None,
                }],
            )
            .await
            .unwrap();

        // Search across all partitions
        let results = manager.search_topic("test", &[0.9, 0.1], 2).await.unwrap();
        assert_eq!(results.len(), 2);
        // First result should be from partition 0 (closer to query)
        assert_eq!(results[0].0, 0); // partition
        assert_eq!(results[0].1, 0); // offset
    }

    #[tokio::test]
    async fn test_topic_stats() {
        let manager = VectorIndexManager::new(VectorIndexConfig::default());

        let config = HnswIndexConfig {
            dimensions: 4,
            ..Default::default()
        };
        manager.register_topic("stats-test", config).await;

        // Add vectors to multiple partitions
        for p in 0..3 {
            for i in 0..5 {
                let vec = vec![p as f32, i as f32, 0.0, 0.0];
                manager
                    .add_vectors(
                        "stats-test",
                        p,
                        vec![VectorEntry {
                            offset: i,
                            vector: vec,
                            text_preview: None,
                        }],
                    )
                    .await
                    .unwrap();
            }
        }

        let stats = manager.get_topic_stats("stats-test").await;
        assert_eq!(stats.partition_count, 3);
        assert_eq!(stats.total_vectors, 15);
        assert_eq!(stats.dimensions, 4);
    }

    #[tokio::test]
    async fn test_clear_operations() {
        let manager = VectorIndexManager::new(VectorIndexConfig::default());

        let config = HnswIndexConfig {
            dimensions: 2,
            ..Default::default()
        };
        manager.register_topic("clear-test", config).await;

        manager
            .add_vectors(
                "clear-test",
                0,
                vec![VectorEntry {
                    offset: 0,
                    vector: vec![1.0, 0.0],
                    text_preview: None,
                }],
            )
            .await
            .unwrap();

        manager
            .add_vectors(
                "clear-test",
                1,
                vec![VectorEntry {
                    offset: 0,
                    vector: vec![0.0, 1.0],
                    text_preview: None,
                }],
            )
            .await
            .unwrap();

        // Clear one partition
        manager.clear_partition("clear-test", 0).await;
        assert!(manager.get_partition_stats("clear-test", 0).await.is_none());
        assert!(manager.get_partition_stats("clear-test", 1).await.is_some());

        // Clear all
        manager.clear_topic("clear-test").await;
        let stats = manager.get_topic_stats("clear-test").await;
        assert_eq!(stats.partition_count, 0);
    }

    #[tokio::test]
    async fn test_dimension_mismatch() {
        let manager = VectorIndexManager::new(VectorIndexConfig::default());

        let config = HnswIndexConfig {
            dimensions: 3,
            ..Default::default()
        };
        manager.register_topic("dim-test", config).await;

        // Try to add wrong dimension
        let result = manager
            .add_vectors(
                "dim-test",
                0,
                vec![VectorEntry {
                    offset: 0,
                    vector: vec![1.0, 0.0], // Only 2 dimensions
                    text_preview: None,
                }],
            )
            .await;

        // Should succeed but log warning (vector not added)
        assert!(result.is_ok());
        let stats = manager.get_partition_stats("dim-test", 0).await;
        // Partition created but no vectors added due to dimension mismatch
        assert!(stats.is_none() || stats.unwrap().vector_count == 0);
    }

    #[test]
    fn test_distance_metrics() {
        let config = HnswIndexConfig {
            dimensions: 3,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);
        index.add_vector(1, vec![1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.0, 1.0, 0.0]).unwrap();

        let query = &[0.9, 0.1, 0.0];
        let results = index.search(query, 2);

        // Vector 1 should be closer
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_cosine_distance() {
        let config = HnswIndexConfig {
            dimensions: 2,
            metric: DistanceMetric::Cosine,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);
        index.add_vector(1, vec![1.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.707, 0.707]).unwrap(); // 45 degrees

        let query = &[1.0, 0.0];
        let results = index.search(query, 2);

        // Vector 1 should be closest (same direction)
        assert_eq!(results[0].0, 1);
        assert!(results[0].1 < 0.01); // Nearly 0 distance
    }

    // ========== HNSW Index Tests ==========

    #[test]
    fn test_hnsw_build_and_search() {
        let config = HnswIndexConfig {
            dimensions: 3,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Add vectors
        index.add_vector(1, vec![1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.0, 1.0, 0.0]).unwrap();
        index.add_vector(3, vec![0.0, 0.0, 1.0]).unwrap();

        // Before build, should use brute force
        assert!(!index.is_built());

        // Build HNSW index
        index.build().unwrap();
        assert!(index.is_built());

        // Search using HNSW
        let results = index.search(&[0.9, 0.1, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1); // Closest to first vector
    }

    #[test]
    fn test_hnsw_cosine_search() {
        let config = HnswIndexConfig {
            dimensions: 3,
            metric: DistanceMetric::Cosine,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Normalized vectors
        index.add_vector(1, vec![1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.707, 0.707, 0.0]).unwrap();  // 45 degrees
        index.add_vector(3, vec![0.0, 1.0, 0.0]).unwrap();      // 90 degrees

        index.build().unwrap();
        assert!(index.is_built());

        // Query along x-axis should find vector 1 closest
        let results = index.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results[0].0, 1);
        assert!(results[0].1 < 0.01); // Should be very close to 0
    }

    #[test]
    fn test_hnsw_dot_product_search() {
        let config = HnswIndexConfig {
            dimensions: 3,
            metric: DistanceMetric::DotProduct,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        index.add_vector(1, vec![1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![2.0, 0.0, 0.0]).unwrap();  // Larger magnitude
        index.add_vector(3, vec![0.0, 1.0, 0.0]).unwrap();  // Orthogonal

        index.build().unwrap();

        // Query along x-axis - vector 2 should have highest dot product
        let results = index.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results[0].0, 2);
        assert!((results[0].1 - 2.0).abs() < 0.01); // Dot product should be 2.0
    }

    #[test]
    fn test_vo2_add_does_not_invalidate_hnsw() {
        let config = HnswIndexConfig {
            dimensions: 2,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        index.add_vector(1, vec![1.0, 0.0]).unwrap();
        index.build().unwrap();
        assert!(index.is_built());

        // VO-2: Adding a new vector should NOT invalidate the built HNSW
        index.add_vector(2, vec![0.0, 1.0]).unwrap();
        assert!(index.is_built()); // HNSW still valid
        assert_eq!(index.pending_vectors.len(), 1); // New vector in pending
        assert_eq!(index.built_vectors.len(), 1); // Original in built

        // Two-tier search finds both vectors
        let results = index.search(&[0.5, 0.5], 2);
        assert_eq!(results.len(), 2);

        // Rebuild merges pending into built
        index.build().unwrap();
        assert!(index.is_built());
        assert_eq!(index.built_vectors.len(), 2);
        assert_eq!(index.pending_vectors.len(), 0);
    }

    #[test]
    fn test_hnsw_brute_force_fallback() {
        let config = HnswIndexConfig {
            dimensions: 3,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        index.add_vector(1, vec![1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.0, 1.0, 0.0]).unwrap();

        // Search without building should use brute force
        assert!(!index.is_built());
        let results = index.search(&[0.9, 0.1, 0.0], 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_partition_index_stats_with_hnsw() {
        let config = HnswIndexConfig {
            dimensions: 2,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        index.add_vector(1, vec![1.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.0, 1.0]).unwrap();

        // Before build: all vectors are pending
        let stats = index.stats();
        assert_eq!(stats.vector_count, 2);
        assert_eq!(stats.pending_count, 2);
        assert!(!stats.is_built);

        // After build: pending merged into built
        index.build().unwrap();
        let stats = index.stats();
        assert_eq!(stats.vector_count, 2);
        assert_eq!(stats.pending_count, 0); // All merged into built
        assert!(stats.is_built);

        // VO-2: Add more vectors — they go to pending, HNSW stays valid
        index.add_vector(3, vec![0.5, 0.5]).unwrap();
        let stats = index.stats();
        assert_eq!(stats.vector_count, 3);
        assert_eq!(stats.pending_count, 1); // One new pending
        assert!(stats.is_built); // HNSW still valid
    }

    #[tokio::test]
    async fn test_vector_index_manager_build() {
        let config = VectorIndexConfig::default();
        let manager = VectorIndexManager::new(config);

        let hnsw_config = HnswIndexConfig {
            dimensions: 3,
            ..Default::default()
        };
        manager.register_topic("test-topic", hnsw_config).await;

        // Add vectors
        let entries = vec![
            VectorEntry {
                offset: 0,
                vector: vec![1.0, 0.0, 0.0],
                text_preview: Some("hello".to_string()),
            },
            VectorEntry {
                offset: 1,
                vector: vec![0.0, 1.0, 0.0],
                text_preview: Some("world".to_string()),
            },
        ];
        manager.add_vectors("test-topic", 0, entries).await.unwrap();

        // Check stats before build
        let stats = manager.get_partition_stats("test-topic", 0).await.unwrap();
        assert!(!stats.is_built);

        // Build the index
        let built = manager.build_partition_index("test-topic", 0).await.unwrap();
        assert!(built);

        // Check stats after build
        let stats = manager.get_partition_stats("test-topic", 0).await.unwrap();
        assert!(stats.is_built);

        // Search should use HNSW
        let results = manager
            .search("test-topic", 0, &[0.9, 0.1, 0.0], 1)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0); // Closest to first vector
    }

    #[tokio::test]
    async fn test_vector_index_manager_build_all() {
        let config = VectorIndexConfig::default();
        let manager = VectorIndexManager::new(config);

        let hnsw_config = HnswIndexConfig {
            dimensions: 2,
            ..Default::default()
        };
        manager.register_topic("test", hnsw_config).await;

        // Add vectors to multiple partitions
        for p in 0..3 {
            manager
                .add_vectors(
                    "test",
                    p,
                    vec![VectorEntry {
                        offset: 0,
                        vector: vec![1.0, 0.0],
                        text_preview: None,
                    }],
                )
                .await
                .unwrap();
        }

        // Build all indexes
        let built = manager.build_all_indexes().await.unwrap();
        assert_eq!(built, 3);

        // All should be built now
        for p in 0..3 {
            let stats = manager.get_partition_stats("test", p).await.unwrap();
            assert!(stats.is_built);
        }
    }

    // ========== VectorSearchFilters Tests ==========

    #[test]
    fn test_filters_default() {
        let filters = VectorSearchFilters::default();
        assert!(filters.partitions.is_none());
        assert!(filters.min_offset.is_none());
        assert!(filters.max_offset.is_none());
        assert!(filters.matches_offset(0));
        assert!(filters.matches_offset(1000));
        assert!(filters.matches_partition(0));
        assert!(filters.matches_partition(10));
    }

    #[test]
    fn test_filters_partition() {
        let filters = VectorSearchFilters::new().with_partition(5);
        assert!(filters.matches_partition(5));
        assert!(!filters.matches_partition(0));
        assert!(!filters.matches_partition(10));
    }

    #[test]
    fn test_filters_partitions() {
        let filters = VectorSearchFilters::new().with_partitions(vec![0, 2, 4]);
        assert!(filters.matches_partition(0));
        assert!(filters.matches_partition(2));
        assert!(filters.matches_partition(4));
        assert!(!filters.matches_partition(1));
        assert!(!filters.matches_partition(3));
    }

    #[test]
    fn test_filters_offset_range() {
        let filters = VectorSearchFilters::new().with_offset_range(100, 200);
        assert!(!filters.matches_offset(50));
        assert!(!filters.matches_offset(99));
        assert!(filters.matches_offset(100));
        assert!(filters.matches_offset(150));
        assert!(filters.matches_offset(200));
        assert!(!filters.matches_offset(201));
        assert!(!filters.matches_offset(1000));
    }

    #[test]
    fn test_filters_min_offset() {
        let filters = VectorSearchFilters::new().with_min_offset(100);
        assert!(!filters.matches_offset(50));
        assert!(!filters.matches_offset(99));
        assert!(filters.matches_offset(100));
        assert!(filters.matches_offset(1000));
    }

    #[test]
    fn test_filters_max_offset() {
        let filters = VectorSearchFilters::new().with_max_offset(200);
        assert!(filters.matches_offset(0));
        assert!(filters.matches_offset(100));
        assert!(filters.matches_offset(200));
        assert!(!filters.matches_offset(201));
    }

    #[test]
    fn test_filters_chained() {
        let filters = VectorSearchFilters::new()
            .with_partition(3)
            .with_offset_range(50, 150);

        assert!(filters.matches_partition(3));
        assert!(!filters.matches_partition(0));
        assert!(filters.matches_offset(100));
        assert!(!filters.matches_offset(200));
    }

    // ========== VectorSearchResult Tests ==========

    #[test]
    fn test_search_result_creation() {
        let result = VectorSearchResult {
            topic: "test-topic".to_string(),
            partition: 2,
            offset: 1000,
            score: 0.123,
            text_preview: Some("Hello world".to_string()),
            embedding: None,
        };

        assert_eq!(result.topic, "test-topic");
        assert_eq!(result.partition, 2);
        assert_eq!(result.offset, 1000);
        assert!((result.score - 0.123).abs() < 0.001);
        assert_eq!(result.text_preview, Some("Hello world".to_string()));
    }

    // ========== VO-3: Scalar Quantization Tests ==========

    #[test]
    fn test_scalar_quantizer_fit() {
        let vectors = vec![
            (1, vec![0.0, -1.0, 0.5]),
            (2, vec![1.0, 1.0, -0.5]),
            (3, vec![0.5, 0.0, 0.0]),
        ];
        let q = ScalarQuantizer::fit(&vectors, 3);

        assert_eq!(q.dimensions, 3);
        assert!((q.min_vals[0] - 0.0).abs() < f32::EPSILON);
        assert!((q.max_vals[0] - 1.0).abs() < f32::EPSILON);
        assert!((q.min_vals[1] - (-1.0)).abs() < f32::EPSILON);
        assert!((q.max_vals[1] - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_scalar_quantizer_roundtrip() {
        let vectors = vec![
            (1, vec![0.0, 0.5, 1.0]),
            (2, vec![1.0, 0.0, 0.5]),
        ];
        let q = ScalarQuantizer::fit(&vectors, 3);

        let original = &[0.5, 0.25, 0.75];
        let quantized = q.quantize_uint8(original);
        let reconstructed = q.dequantize_uint8(&quantized);

        // uint8 quantization has ~1/255 = ~0.004 precision per dimension
        for i in 0..3 {
            assert!(
                (original[i] - reconstructed[i]).abs() < 0.01,
                "Dim {}: expected ~{}, got {}",
                i, original[i], reconstructed[i]
            );
        }
    }

    #[test]
    fn test_scalar_quantizer_uint8_range() {
        let vectors = vec![
            (1, vec![-10.0, 0.0]),
            (2, vec![10.0, 100.0]),
        ];
        let q = ScalarQuantizer::fit(&vectors, 2);

        // Minimum maps to 0
        let min_q = q.quantize_uint8(&[-10.0, 0.0]);
        assert_eq!(min_q[0], 0);
        assert_eq!(min_q[1], 0);

        // Maximum maps to 255
        let max_q = q.quantize_uint8(&[10.0, 100.0]);
        assert_eq!(max_q[0], 255);
        assert_eq!(max_q[1], 255);

        // Midpoint maps to ~127-128
        let mid_q = q.quantize_uint8(&[0.0, 50.0]);
        assert!((mid_q[0] as i16 - 127).abs() <= 1);
        assert!((mid_q[1] as i16 - 127).abs() <= 1);
    }

    #[test]
    fn test_asymmetric_cosine_distance() {
        let vectors = vec![
            (1, vec![1.0, 0.0, 0.0]),
            (2, vec![0.0, 1.0, 0.0]),
        ];
        let q = ScalarQuantizer::fit(&vectors, 3);

        let query = &[1.0, 0.0, 0.0];
        let v1_quantized = q.quantize_uint8(&[1.0, 0.0, 0.0]);
        let v2_quantized = q.quantize_uint8(&[0.0, 1.0, 0.0]);

        let d1 = q.asymmetric_cosine_distance(query, &v1_quantized);
        let d2 = q.asymmetric_cosine_distance(query, &v2_quantized);

        // v1 should be much closer to query than v2
        assert!(d1 < d2, "d1={} should be < d2={}", d1, d2);
        assert!(d1 < 0.1, "Same direction should have near-zero cosine distance, got {}", d1);
    }

    #[test]
    fn test_partition_index_uint8_quantization() {
        let config = HnswIndexConfig {
            dimensions: 3,
            metric: DistanceMetric::Cosine,
            quantization: QuantizationMode::Uint8,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Add some vectors
        index.add_vector(1, vec![1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.0, 1.0, 0.0]).unwrap();
        index.add_vector(3, vec![0.707, 0.707, 0.0]).unwrap();

        // Build triggers quantizer training
        index.build().unwrap();
        assert!(index.quantizer.is_some());
        assert_eq!(index.quantized_built.len(), 3);

        // Search should still find correct nearest neighbor
        let results = index.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1); // Closest to first vector

        // Add pending vector — should also be quantized
        index.add_vector(4, vec![0.0, 0.0, 1.0]).unwrap();
        assert_eq!(index.quantized_pending.len(), 1);

        // Search finds all 4 vectors
        let results = index.search(&[0.0, 0.0, 1.0], 4);
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].0, 4); // Closest to the query
    }

    #[test]
    fn test_quantization_recall() {
        // Test that uint8 quantization maintains reasonable recall
        let config_f32 = HnswIndexConfig {
            dimensions: 4,
            metric: DistanceMetric::Cosine,
            quantization: QuantizationMode::F32,
            ..Default::default()
        };
        let config_uint8 = HnswIndexConfig {
            dimensions: 4,
            metric: DistanceMetric::Cosine,
            quantization: QuantizationMode::Uint8,
            ..Default::default()
        };

        let mut index_f32 = PartitionIndex::new(config_f32);
        let mut index_uint8 = PartitionIndex::new(config_uint8);

        let vectors = vec![
            (1, vec![1.0, 0.0, 0.0, 0.0]),
            (2, vec![0.0, 1.0, 0.0, 0.0]),
            (3, vec![0.0, 0.0, 1.0, 0.0]),
            (4, vec![0.0, 0.0, 0.0, 1.0]),
            (5, vec![0.707, 0.707, 0.0, 0.0]),
            (6, vec![0.577, 0.577, 0.577, 0.0]),
        ];

        for (id, vec) in &vectors {
            index_f32.add_vector(*id as i64, vec.clone()).unwrap();
            index_uint8.add_vector(*id as i64, vec.clone()).unwrap();
        }

        index_f32.build().unwrap();
        index_uint8.build().unwrap();

        let query = &[0.8, 0.6, 0.0, 0.0];
        let results_f32 = index_f32.search(query, 3);
        let results_uint8 = index_uint8.search(query, 3);

        // Both should return same top result
        assert_eq!(results_f32[0].0, results_uint8[0].0,
            "Top-1 result should match: f32={}, uint8={}", results_f32[0].0, results_uint8[0].0);
    }

    #[test]
    fn test_quantizer_empty_vectors() {
        // Fit with empty vector set should produce valid quantizer
        let q = ScalarQuantizer::fit(&[], 3);
        assert_eq!(q.dimensions, 3);
        // Should not panic when quantizing
        let result = q.quantize_uint8(&[0.5, 0.5, 0.5]);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_vector_memory_bytes() {
        let config = HnswIndexConfig {
            dimensions: 4,
            metric: DistanceMetric::Cosine,
            quantization: QuantizationMode::Uint8,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Empty index has 0 memory
        assert_eq!(index.vector_memory_bytes(), 0);

        // Add 2 vectors (4 dims, f32 = 16 bytes each)
        index.add_vector(1, vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, vec![0.0, 1.0, 0.0, 0.0]).unwrap();
        assert_eq!(index.vector_memory_bytes(), 2 * 4 * 4); // 2 vectors × 4 dims × 4 bytes

        // Build: vectors move to built + quantized copies created
        index.build().unwrap();
        let mem = index.vector_memory_bytes();
        // f32: 2 × 4 × 4 = 32 bytes
        // uint8: 2 × 4 × 1 = 8 bytes
        // quantizer: 4 × 4 × 2 + 8 = 40 bytes (min_vals + max_vals + usize)
        assert!(mem > 32, "Should include quantized vectors, got {}", mem);
    }

    // =========================================================================
    // VO-6: Matryoshka Dimension Reduction Tests
    // =========================================================================

    #[test]
    fn test_truncate_vec() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(truncate_vec(&v, 3), vec![1.0, 2.0, 3.0]);
        assert_eq!(truncate_vec(&v, 6), v); // No truncation
        assert_eq!(truncate_vec(&v, 10), v); // Beyond length = clone
        assert_eq!(truncate_vec(&v, 1), vec![1.0]);
    }

    #[test]
    fn test_hnsw_config_effective_search_dims() {
        let mut config = HnswIndexConfig::default(); // 1536 dims
        assert_eq!(config.effective_search_dims(), 1536); // 0 = use full
        assert!(!config.is_matryoshka_enabled());

        config.search_dimensions = 512;
        assert_eq!(config.effective_search_dims(), 512);
        assert!(config.is_matryoshka_enabled());

        config.search_dimensions = 1536; // Same as full = no truncation
        assert_eq!(config.effective_search_dims(), 1536);
        assert!(!config.is_matryoshka_enabled());

        config.search_dimensions = 2000; // Larger than dims = no truncation
        assert_eq!(config.effective_search_dims(), 1536);
        assert!(!config.is_matryoshka_enabled());
    }

    #[test]
    fn test_matryoshka_search_truncated_dims() {
        // Create index with 6-dim vectors but search at 3-dim
        let config = HnswIndexConfig {
            dimensions: 6,
            metric: DistanceMetric::Cosine,
            search_dimensions: 3,
            adaptive_retrieval: false,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Add vectors where first 3 dims determine similarity
        // Vec A: [1, 0, 0, ...] and Vec B: [0, 1, 0, ...]
        // At 3d, query [1,0,0] should prefer A. The trailing dims are noise.
        let _ = index.add_vector(0, vec![1.0, 0.0, 0.0, 0.5, 0.5, 0.5]);
        let _ = index.add_vector(1, vec![0.0, 1.0, 0.0, 0.5, 0.5, 0.5]);
        let _ = index.add_vector(2, vec![0.9, 0.1, 0.0, 0.0, 0.0, 0.0]);
        index.build().unwrap();

        // Query: first 3 dims close to vec 0 and vec 2
        let query = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let results = index.search(&query, 2);

        assert_eq!(results.len(), 2);
        // The closest should be offset 0 or 2 (both point in similar direction at 3d)
        let offsets: Vec<i64> = results.iter().map(|(o, _)| *o).collect();
        assert!(offsets.contains(&0) || offsets.contains(&2),
            "Expected offsets 0 or 2 in top-2, got {:?}", offsets);
    }

    #[test]
    fn test_matryoshka_adaptive_retrieval() {
        // With adaptive retrieval, candidates from truncated HNSW are re-scored
        // at full dimensions, potentially reordering results.
        let config = HnswIndexConfig {
            dimensions: 6,
            metric: DistanceMetric::Cosine,
            search_dimensions: 3,
            adaptive_retrieval: true,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Vec A: at 3d looks like [1,0,0], at 6d looks like [1,0,0,0,0,0]
        // Vec B: at 3d looks like [1,0,0], at 6d looks like [1,0,0,1,0,0] — different!
        // Vec C: at 3d looks like [0,1,0], clearly different
        let _ = index.add_vector(0, vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let _ = index.add_vector(1, vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let _ = index.add_vector(2, vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        index.build().unwrap();

        // Query matches A at full dims
        let query = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let results = index.search(&query, 2);

        assert_eq!(results.len(), 2);
        // With adaptive, offset 0 should rank first (exact match at full dims)
        assert_eq!(results[0].0, 0, "Offset 0 should be closest at full dims");
    }

    #[test]
    fn test_partition_index_dedup() {
        let config = HnswIndexConfig {
            dimensions: 3,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Add vector for offset 42
        index.add_vector(42, vec![1.0, 0.0, 0.0]).unwrap();
        assert_eq!(index.vector_count(), 1);

        // Add same offset again — should be deduplicated
        index.add_vector(42, vec![0.0, 1.0, 0.0]).unwrap();
        assert_eq!(index.vector_count(), 1, "Duplicate offset should be skipped");

        // Different offset should be added
        index.add_vector(43, vec![0.0, 0.0, 1.0]).unwrap();
        assert_eq!(index.vector_count(), 2);
    }

    #[test]
    fn test_partition_index_dedup_across_build() {
        let config = HnswIndexConfig {
            dimensions: 2,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        // Add 100+ vectors to trigger first build
        for i in 0..150 {
            index.add_vector(i, vec![i as f32, 0.0]).unwrap();
        }
        assert_eq!(index.vector_count(), 150);

        // Build HNSW (merges pending into built)
        index.build().unwrap();
        assert_eq!(index.vector_count(), 150);

        // Try adding duplicates of already-built offsets
        index.add_vector(0, vec![999.0, 999.0]).unwrap();
        index.add_vector(50, vec![999.0, 999.0]).unwrap();
        index.add_vector(149, vec![999.0, 999.0]).unwrap();
        assert_eq!(index.vector_count(), 150, "Duplicates of built vectors should be skipped");

        // New offset should still work
        index.add_vector(150, vec![150.0, 0.0]).unwrap();
        assert_eq!(index.vector_count(), 151);
    }

    #[tokio::test]
    async fn test_partitions_with_vectors() {
        let manager = VectorIndexManager::new(VectorIndexConfig::default());
        let config = HnswIndexConfig {
            dimensions: 2,
            ..Default::default()
        };
        manager.register_topic("test", config).await;

        // Initially no partitions have vectors
        let partitions = manager.partitions_with_vectors("test").await;
        assert!(partitions.is_empty());

        // Add vectors to partition 0 only
        manager
            .add_vectors("test", 0, vec![VectorEntry {
                offset: 0,
                vector: vec![1.0, 0.0],
                text_preview: None,
            }])
            .await
            .unwrap();

        let partitions = manager.partitions_with_vectors("test").await;
        assert_eq!(partitions, vec![0]);

        // Add vectors to partition 2 (skip 1)
        manager
            .add_vectors("test", 2, vec![VectorEntry {
                offset: 0,
                vector: vec![0.0, 1.0],
                text_preview: None,
            }])
            .await
            .unwrap();

        let partitions = manager.partitions_with_vectors("test").await;
        assert_eq!(partitions, vec![0, 2]);

        // Partition 1 should NOT be in the list
        assert!(!partitions.contains(&1));
    }
}
