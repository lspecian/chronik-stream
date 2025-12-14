//! Vector search with HNSW (Hierarchical Navigable Small World) algorithm.
//!
//! This module provides efficient O(log n) approximate nearest neighbor search
//! using the instant-distance crate's HNSW implementation.
//!
//! ## Performance Characteristics
//! - Build time: O(n log n) for n vectors
//! - Search time: O(log n) per query
//! - Memory: O(n * M) where M is the connectivity parameter
//!
//! ## Example
//! ```ignore
//! use chronik_storage::vector_search::{RealHnswIndex, HnswConfig, DistanceMetric};
//!
//! let config = HnswConfig::default();
//! let mut index = RealHnswIndex::new(config);
//!
//! // Add vectors
//! index.add_vector(1, &[1.0, 0.0, 0.0])?;
//! index.add_vector(2, &[0.0, 1.0, 0.0])?;
//!
//! // Build the index (required before search)
//! index.build()?;
//!
//! // Search for nearest neighbors
//! let results = index.search(&[0.9, 0.1, 0.0], 2)?;
//! ```

use anyhow::Result;
use serde::{Deserialize, Serialize};
use async_trait::async_trait;
use instant_distance::{Builder, Search, HnswMap};
use tracing::{debug, info};

/// Trait for vector index implementations
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Add a vector to the index
    fn add_vector(&mut self, id: u64, vector: &[f32]) -> Result<()>;

    /// Search for k nearest neighbors
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>>;

    /// Search with additional filtering
    fn search_with_filter(
        &self,
        query: &[f32],
        k: usize,
        _filter: &serde_json::Value
    ) -> Result<Vec<(u64, f32)>> {
        // Default implementation ignores filter
        self.search(query, k)
    }

    /// Serialize the index to bytes
    fn serialize(&self) -> Result<Vec<u8>>;

    /// Deserialize the index from bytes
    fn deserialize(data: &[u8]) -> Result<Self> where Self: Sized;

    /// Get the number of dimensions this index expects
    fn dimensions(&self) -> usize;

    /// Get the number of vectors in the index
    fn count(&self) -> usize;
}

/// Vector index data stored in segments
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexData {
    /// Type of index (e.g., "hnsw", "ivf", "flat")
    pub index_type: String,
    /// Number of dimensions
    pub dimensions: usize,
    /// Distance metric used
    pub metric: DistanceMetric,
    /// Serialized index data
    pub data: Vec<u8>,
    /// Index-specific metadata
    pub metadata: serde_json::Value,
}

/// Distance metrics for vector similarity
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum DistanceMetric {
    /// Euclidean distance (L2)
    Euclidean,
    /// Cosine similarity
    Cosine,
    /// Inner product
    InnerProduct,
}

/// Request for hybrid text and vector search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridSearchRequest {
    /// Traditional text query (Elasticsearch DSL compatible)
    pub text_query: Option<serde_json::Value>,
    /// Vector similarity query
    pub vector_query: Option<VectorQuery>,
    /// How to combine text and vector results
    pub fusion_method: FusionMethod,
    /// Number of results to return
    pub size: usize,
    /// Offset for pagination
    pub from: usize,
}

/// Vector query parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorQuery {
    /// Query vector
    pub vector: Vec<f32>,
    /// Number of nearest neighbors to find
    pub k: usize,
    /// Optional filter to apply before vector search
    pub filter: Option<serde_json::Value>,
    /// Field name containing vectors
    pub field: String,
    /// Minimum similarity score threshold
    pub min_score: Option<f32>,
}

/// Methods for combining text and vector search results
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FusionMethod {
    /// Weighted rank fusion
    RankFusion {
        /// Weight for text score (0.0 to 1.0)
        text_weight: f32,
        /// Weight for vector score (0.0 to 1.0)
        vector_weight: f32,
    },
    /// Rerank text results using vector similarity
    VectorRerank {
        /// Number of text results to rerank
        rerank_top_k: usize,
    },
    /// Use vector search to boost text scores
    VectorBoost {
        /// Boost factor for vector matches
        boost_factor: f32,
    },
}

/// Result from hybrid search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridSearchResult {
    /// Document ID
    pub id: String,
    /// Text search score (if available)
    pub text_score: Option<f32>,
    /// Vector similarity score (if available)
    pub vector_score: Option<f32>,
    /// Combined score
    pub final_score: f32,
    /// Document source
    pub source: serde_json::Value,
}

/// Configuration for HNSW index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswConfig {
    /// Number of dimensions (required)
    pub dimensions: usize,
    /// Distance metric
    pub metric: DistanceMetric,
    /// Number of bi-directional links per node (M parameter)
    /// Higher = better recall but more memory and slower build
    pub m: usize,
    /// Size of dynamic candidate list during construction (ef_construction)
    /// Higher = better index quality but slower build
    pub ef_construction: usize,
    /// Size of dynamic candidate list during search (ef)
    /// Higher = better recall but slower search
    pub ef_search: usize,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            dimensions: 768,        // Common embedding size (BERT, OpenAI ada-002)
            metric: DistanceMetric::Euclidean,
            m: 16,                  // 16 is a good default balance
            ef_construction: 200,   // Higher for better build quality
            ef_search: 50,          // Balance between speed and accuracy
        }
    }
}

/// Point wrapper for instant-distance that implements the Point trait
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

/// Point wrapper for cosine distance
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

/// Point wrapper for inner product (dot product)
#[derive(Clone)]
struct DotProductPoint(Vec<f32>);

impl instant_distance::Point for DotProductPoint {
    fn distance(&self, other: &Self) -> f32 {
        // Negate because instant-distance minimizes distance
        // and we want to maximize dot product
        -self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum::<f32>()
    }
}

/// Serializable data for HNSW index
#[derive(Serialize, Deserialize)]
struct HnswSerializedData {
    config: HnswConfig,
    vectors: Vec<(u64, Vec<f32>)>,
}

/// Real HNSW index implementation using instant-distance
///
/// This index builds an HNSW graph for O(log n) approximate nearest neighbor search.
/// The index must be built after adding vectors before search can be performed.
pub struct RealHnswIndex {
    config: HnswConfig,
    /// Pending vectors to be added to the index
    pending_vectors: Vec<(u64, Vec<f32>)>,
    /// Built HNSW index (Euclidean) - maps points to user IDs
    euclidean_index: Option<HnswMap<EuclideanPoint, u64>>,
    /// Built HNSW index (Cosine) - maps points to user IDs
    cosine_index: Option<HnswMap<CosinePoint, u64>>,
    /// Built HNSW index (Dot Product) - maps points to user IDs
    dot_index: Option<HnswMap<DotProductPoint, u64>>,
}

impl RealHnswIndex {
    /// Create a new HNSW index with the given configuration
    pub fn new(config: HnswConfig) -> Self {
        Self {
            config,
            pending_vectors: Vec::new(),
            euclidean_index: None,
            cosine_index: None,
            dot_index: None,
        }
    }

    /// Create a new HNSW index with default configuration for given dimensions
    pub fn with_dimensions(dimensions: usize) -> Self {
        Self::new(HnswConfig {
            dimensions,
            ..Default::default()
        })
    }

    /// Build the HNSW graph from pending vectors
    ///
    /// This must be called after adding vectors and before searching.
    /// Building is an O(n log n) operation.
    pub fn build(&mut self) -> Result<()> {
        if self.pending_vectors.is_empty() {
            return Ok(());
        }

        info!("Building HNSW index with {} vectors, {} dimensions, metric={:?}",
              self.pending_vectors.len(), self.config.dimensions, self.config.metric);

        // Extract IDs for mapping
        let ids: Vec<u64> = self.pending_vectors.iter().map(|(id, _)| *id).collect();

        match self.config.metric {
            DistanceMetric::Euclidean => {
                let points: Vec<EuclideanPoint> = self.pending_vectors
                    .iter()
                    .map(|(_, v)| EuclideanPoint(v.clone()))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.euclidean_index = Some(hnsw);
            }
            DistanceMetric::Cosine => {
                let points: Vec<CosinePoint> = self.pending_vectors
                    .iter()
                    .map(|(_, v)| CosinePoint(v.clone()))
                    .collect();

                let ids: Vec<u64> = self.pending_vectors.iter().map(|(id, _)| *id).collect();
                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.cosine_index = Some(hnsw);
            }
            DistanceMetric::InnerProduct => {
                let points: Vec<DotProductPoint> = self.pending_vectors
                    .iter()
                    .map(|(_, v)| DotProductPoint(v.clone()))
                    .collect();

                let ids: Vec<u64> = self.pending_vectors.iter().map(|(id, _)| *id).collect();
                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.dot_index = Some(hnsw);
            }
        }

        debug!("HNSW index built successfully");
        Ok(())
    }

    /// Check if the index has been built
    pub fn is_built(&self) -> bool {
        match self.config.metric {
            DistanceMetric::Euclidean => self.euclidean_index.is_some(),
            DistanceMetric::Cosine => self.cosine_index.is_some(),
            DistanceMetric::InnerProduct => self.dot_index.is_some(),
        }
    }

    /// Get configuration
    pub fn config(&self) -> &HnswConfig {
        &self.config
    }

    /// Get index statistics
    pub fn stats(&self) -> HnswStats {
        HnswStats {
            vector_count: self.pending_vectors.len(),
            dimensions: self.config.dimensions,
            metric: self.config.metric,
            is_built: self.is_built(),
            m: self.config.m,
            ef_construction: self.config.ef_construction,
            ef_search: self.config.ef_search,
        }
    }
}

/// Statistics about the HNSW index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswStats {
    pub vector_count: usize,
    pub dimensions: usize,
    pub metric: DistanceMetric,
    pub is_built: bool,
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

#[async_trait]
impl VectorIndex for RealHnswIndex {
    fn add_vector(&mut self, id: u64, vector: &[f32]) -> Result<()> {
        if vector.len() != self.config.dimensions {
            return Err(anyhow::anyhow!(
                "Vector dimension mismatch: expected {}, got {}",
                self.config.dimensions,
                vector.len()
            ));
        }

        self.pending_vectors.push((id, vector.to_vec()));

        // Invalidate built index when new vectors are added
        self.euclidean_index = None;
        self.cosine_index = None;
        self.dot_index = None;

        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
        if query.len() != self.config.dimensions {
            return Err(anyhow::anyhow!(
                "Query dimension mismatch: expected {}, got {}",
                self.config.dimensions,
                query.len()
            ));
        }

        if self.pending_vectors.is_empty() {
            return Ok(Vec::new());
        }

        // If index not built, fall back to brute force
        if !self.is_built() {
            debug!("HNSW index not built, using brute force search");
            return self.brute_force_search(query, k);
        }

        let mut search = Search::default();
        let mut results = Vec::with_capacity(k);

        match self.config.metric {
            DistanceMetric::Euclidean => {
                if let Some(ref hnsw) = self.euclidean_index {
                    let query_point = EuclideanPoint(query.to_vec());
                    for candidate in hnsw.search(&query_point, &mut search).take(k) {
                        results.push((*candidate.value, candidate.distance));
                    }
                }
            }
            DistanceMetric::Cosine => {
                if let Some(ref hnsw) = self.cosine_index {
                    let query_point = CosinePoint(query.to_vec());
                    for candidate in hnsw.search(&query_point, &mut search).take(k) {
                        results.push((*candidate.value, candidate.distance));
                    }
                }
            }
            DistanceMetric::InnerProduct => {
                if let Some(ref hnsw) = self.dot_index {
                    let query_point = DotProductPoint(query.to_vec());
                    for candidate in hnsw.search(&query_point, &mut search).take(k) {
                        // Negate back to get actual dot product score
                        results.push((*candidate.value, -candidate.distance));
                    }
                }
            }
        }

        Ok(results)
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        let data = HnswSerializedData {
            config: self.config.clone(),
            vectors: self.pending_vectors.clone(),
        };
        Ok(bincode::serialize(&data)?)
    }

    fn deserialize(data: &[u8]) -> Result<Self> {
        let serialized: HnswSerializedData = bincode::deserialize(data)?;
        let mut index = Self::new(serialized.config);
        index.pending_vectors = serialized.vectors;
        // Rebuild the index
        index.build()?;
        Ok(index)
    }

    fn dimensions(&self) -> usize {
        self.config.dimensions
    }

    fn count(&self) -> usize {
        self.pending_vectors.len()
    }
}

impl RealHnswIndex {
    /// Brute force search fallback when index is not built
    fn brute_force_search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
        let mut distances: Vec<(u64, f32)> = self.pending_vectors
            .iter()
            .map(|(id, vec)| {
                let dist = match self.config.metric {
                    DistanceMetric::Euclidean => {
                        query.iter()
                            .zip(vec.iter())
                            .map(|(a, b)| (a - b).powi(2))
                            .sum::<f32>()
                            .sqrt()
                    }
                    DistanceMetric::Cosine => {
                        let dot: f32 = query.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
                        let norm_a: f32 = query.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
                        let norm_b: f32 = vec.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
                        if norm_a == 0.0 || norm_b == 0.0 {
                            1.0
                        } else {
                            1.0 - (dot / (norm_a * norm_b))
                        }
                    }
                    DistanceMetric::InnerProduct => {
                        -query.iter().zip(vec.iter()).map(|(a, b)| a * b).sum::<f32>()
                    }
                };
                (*id, dist)
            })
            .collect();

        distances.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        distances.truncate(k);

        Ok(distances)
    }
}

// Keep the old HnswIndex as an alias for backward compatibility
/// Legacy HNSW index (alias for RealHnswIndex)
pub type HnswIndex = RealHnswIndex;

/// Factory for creating vector indices
pub struct VectorIndexFactory;

impl VectorIndexFactory {
    /// Create a vector index from type string
    pub fn create(
        index_type: &str,
        dimensions: usize,
        metric: DistanceMetric,
        params: serde_json::Value,
    ) -> Result<Box<dyn VectorIndex>> {
        match index_type {
            "hnsw" => {
                let m = params.get("m").and_then(|v| v.as_u64()).unwrap_or(16) as usize;
                let ef_construction = params.get("ef_construction")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(200) as usize;
                let ef_search = params.get("ef_search")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50) as usize;

                let config = HnswConfig {
                    dimensions,
                    metric,
                    m,
                    ef_construction,
                    ef_search,
                };
                Ok(Box::new(RealHnswIndex::new(config)))
            }
            // Future: Add more index types like IVF, Flat, etc.
            _ => Err(anyhow::anyhow!("Unknown vector index type: {}", index_type)),
        }
    }

    /// Create a vector index from serialized data
    pub fn from_data(data: &VectorIndexData) -> Result<Box<dyn VectorIndex>> {
        match data.index_type.as_str() {
            "hnsw" => Ok(Box::new(RealHnswIndex::deserialize(&data.data)?)),
            _ => Err(anyhow::anyhow!("Unknown vector index type: {}", data.index_type)),
        }
    }
}

/// Score fusion utilities
pub struct ScoreFusion;

impl ScoreFusion {
    /// Combine text and vector scores using the specified fusion method
    pub fn fuse_scores(
        text_score: Option<f32>,
        vector_score: Option<f32>,
        method: &FusionMethod,
    ) -> f32 {
        match method {
            FusionMethod::RankFusion { text_weight, vector_weight } => {
                let t_score = text_score.unwrap_or(0.0) * text_weight;
                let v_score = vector_score.unwrap_or(0.0) * vector_weight;
                t_score + v_score
            }
            FusionMethod::VectorRerank { .. } => {
                // In reranking, vector score takes precedence
                vector_score.unwrap_or(text_score.unwrap_or(0.0))
            }
            FusionMethod::VectorBoost { boost_factor } => {
                let base = text_score.unwrap_or(0.0);
                if vector_score.is_some() {
                    base * boost_factor
                } else {
                    base
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_real_hnsw_basic_operations() {
        let config = HnswConfig {
            dimensions: 3,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        // Add vectors
        index.add_vector(1, &[1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, &[0.0, 1.0, 0.0]).unwrap();
        index.add_vector(3, &[0.0, 0.0, 1.0]).unwrap();

        // Build index
        index.build().unwrap();
        assert!(index.is_built());

        // Search
        let results = index.search(&[0.9, 0.1, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1); // Closest to first vector
    }

    #[test]
    fn test_real_hnsw_cosine_distance() {
        let config = HnswConfig {
            dimensions: 3,
            metric: DistanceMetric::Cosine,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        // Normalized vectors
        index.add_vector(1, &[1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, &[0.707, 0.707, 0.0]).unwrap();  // 45 degrees from x-axis
        index.add_vector(3, &[0.0, 1.0, 0.0]).unwrap();      // 90 degrees from x-axis

        index.build().unwrap();

        // Query along x-axis should find vector 1 closest
        let results = index.search(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results[0].0, 1);
        assert!(results[0].1 < 0.01); // Should be very close to 0
    }

    #[test]
    fn test_real_hnsw_inner_product() {
        let config = HnswConfig {
            dimensions: 3,
            metric: DistanceMetric::InnerProduct,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        index.add_vector(1, &[1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, &[2.0, 0.0, 0.0]).unwrap();  // Larger magnitude
        index.add_vector(3, &[0.0, 1.0, 0.0]).unwrap();  // Orthogonal

        index.build().unwrap();

        // Query along x-axis - vector 2 should have highest dot product
        let results = index.search(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results[0].0, 2);
        assert!((results[0].1 - 2.0).abs() < 0.01); // Dot product should be 2.0
    }

    #[test]
    fn test_real_hnsw_serialization() {
        let config = HnswConfig {
            dimensions: 2,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);
        index.add_vector(1, &[1.0, 0.0]).unwrap();
        index.add_vector(2, &[0.0, 1.0]).unwrap();
        index.build().unwrap();

        let serialized = index.serialize().unwrap();
        let deserialized = RealHnswIndex::deserialize(&serialized).unwrap();

        assert_eq!(deserialized.dimensions(), 2);
        assert_eq!(deserialized.count(), 2);
        assert!(deserialized.is_built());

        // Verify search works on deserialized index
        let results = deserialized.search(&[0.9, 0.1], 1).unwrap();
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_hnsw_fallback_brute_force() {
        let config = HnswConfig {
            dimensions: 3,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        index.add_vector(1, &[1.0, 0.0, 0.0]).unwrap();
        index.add_vector(2, &[0.0, 1.0, 0.0]).unwrap();

        // Search without building - should use brute force
        assert!(!index.is_built());
        let results = index.search(&[0.9, 0.1, 0.0], 1).unwrap();
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_hnsw_stats() {
        let config = HnswConfig {
            dimensions: 128,
            m: 32,
            ef_construction: 400,
            ef_search: 100,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        // Create a random 128-dim vector
        let vec: Vec<f32> = (0..128).map(|i| (i as f32) / 128.0).collect();
        index.add_vector(1, &vec).unwrap();

        let stats = index.stats();
        assert_eq!(stats.vector_count, 1);
        assert_eq!(stats.dimensions, 128);
        assert_eq!(stats.m, 32);
        assert_eq!(stats.ef_construction, 400);
        assert_eq!(stats.ef_search, 100);
        assert!(!stats.is_built);

        index.build().unwrap();
        let stats = index.stats();
        assert!(stats.is_built);
    }

    #[test]
    fn test_score_fusion() {
        let text_score = Some(0.8);
        let vector_score = Some(0.9);

        // Test rank fusion
        let fused = ScoreFusion::fuse_scores(
            text_score,
            vector_score,
            &FusionMethod::RankFusion {
                text_weight: 0.4,
                vector_weight: 0.6,
            },
        );
        assert!((fused - 0.86).abs() < 0.001);

        // Test vector boost
        let boosted = ScoreFusion::fuse_scores(
            text_score,
            vector_score,
            &FusionMethod::VectorBoost { boost_factor: 1.5 },
        );
        assert!((boosted - 1.2).abs() < 0.001);
    }

    #[test]
    fn test_dimension_mismatch() {
        let config = HnswConfig {
            dimensions: 3,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        // Try to add wrong dimension vector
        let result = index.add_vector(1, &[1.0, 0.0]);
        assert!(result.is_err());

        // Try to search with wrong dimension query
        index.add_vector(1, &[1.0, 0.0, 0.0]).unwrap();
        let result = index.search(&[1.0, 0.0], 1);
        assert!(result.is_err());
    }
}
