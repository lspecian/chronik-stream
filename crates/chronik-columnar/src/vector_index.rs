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
use tokio::fs;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration, Instant};
use tracing::{debug, error, info, warn};

use chronik_embeddings::config::{HnswConfig as EmbeddingHnswConfig, VectorSearchConfig};
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
    /// Number of dimensions
    pub dimensions: usize,
    /// Distance metric
    pub metric: DistanceMetric,
    /// M parameter (bi-directional links per node)
    pub m: usize,
    /// ef_construction (candidate list size during build)
    pub ef_construction: usize,
    /// ef_search (candidate list size during search)
    pub ef_search: usize,
}

impl Default for HnswIndexConfig {
    fn default() -> Self {
        Self {
            dimensions: 1536, // OpenAI text-embedding-3-small
            metric: DistanceMetric::Cosine,
            m: 16,
            ef_construction: 200,
            ef_search: 50,
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
        }
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

/// Key for identifying a topic-partition index
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicPartitionKey {
    pub topic: String,
    pub partition: i32,
}

impl TopicPartitionKey {
    pub fn new(topic: impl Into<String>, partition: i32) -> Self {
        Self {
            topic: topic.into(),
            partition,
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
}

/// Statistics for a topic across all partitions
#[derive(Debug, Clone, Default)]
pub struct TopicIndexStats {
    /// Number of partitions with indexes
    pub partition_count: usize,
    /// Total vectors across all partitions
    pub total_vectors: usize,
    /// Dimensions of vectors
    pub dimensions: usize,
    /// Per-partition statistics
    pub partitions: HashMap<i32, PartitionIndexStats>,
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
// Partition Index with HNSW Support
// ============================================================================

/// In-memory vector index for a single partition
///
/// Uses instant-distance HNSW algorithm for O(log n) approximate nearest neighbor
/// search when built. Falls back to brute-force O(n) search when not built.
struct PartitionIndex {
    /// HNSW configuration
    config: HnswIndexConfig,
    /// Pending vectors (not yet in built index)
    pending: Vec<(u64, Vec<f32>)>,
    /// Last offset indexed
    last_offset: Option<i64>,
    /// Whether the index needs rebuilding
    needs_rebuild: bool,
    /// Built HNSW index for Euclidean distance
    euclidean_index: Option<HnswMap<EuclideanPoint, u64>>,
    /// Built HNSW index for Cosine distance
    cosine_index: Option<HnswMap<CosinePoint, u64>>,
    /// Built HNSW index for Dot Product distance
    dot_index: Option<HnswMap<DotProductPoint, u64>>,
}

impl PartitionIndex {
    fn new(config: HnswIndexConfig) -> Self {
        Self {
            config,
            pending: Vec::new(),
            last_offset: None,
            needs_rebuild: false,
            euclidean_index: None,
            cosine_index: None,
            dot_index: None,
        }
    }

    fn add_vector(&mut self, offset: i64, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.config.dimensions {
            return Err(anyhow!(
                "Dimension mismatch: expected {}, got {}",
                self.config.dimensions,
                vector.len()
            ));
        }

        self.pending.push((offset as u64, vector));
        self.last_offset = Some(offset);
        self.needs_rebuild = true;

        // Invalidate built index when new vectors are added
        self.euclidean_index = None;
        self.cosine_index = None;
        self.dot_index = None;

        Ok(())
    }

    /// Build the HNSW graph from pending vectors
    ///
    /// This enables O(log n) search instead of O(n) brute-force.
    /// Building is an O(n log n) operation.
    fn build(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        // Already built and no new vectors
        if !self.needs_rebuild && self.is_built() {
            return Ok(());
        }

        debug!(
            "Building HNSW index with {} vectors, {} dimensions, metric={:?}",
            self.pending.len(),
            self.config.dimensions,
            self.config.metric
        );

        // Extract IDs for mapping
        let ids: Vec<u64> = self.pending.iter().map(|(id, _)| *id).collect();

        match self.config.metric {
            DistanceMetric::Euclidean => {
                let points: Vec<EuclideanPoint> = self
                    .pending
                    .iter()
                    .map(|(_, v)| EuclideanPoint(v.clone()))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.euclidean_index = Some(hnsw);
            }
            DistanceMetric::Cosine => {
                let points: Vec<CosinePoint> = self
                    .pending
                    .iter()
                    .map(|(_, v)| CosinePoint(v.clone()))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.cosine_index = Some(hnsw);
            }
            DistanceMetric::DotProduct => {
                let points: Vec<DotProductPoint> = self
                    .pending
                    .iter()
                    .map(|(_, v)| DotProductPoint(v.clone()))
                    .collect();

                let hnsw = Builder::default()
                    .ef_construction(self.config.ef_construction)
                    .build(points, ids);

                self.dot_index = Some(hnsw);
            }
        }

        self.needs_rebuild = false;
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
            vector_count: self.pending.len(),
            pending_count: if self.is_built() { 0 } else { self.pending.len() },
            is_built: self.is_built(),
            last_offset: self.last_offset,
            dimensions: self.config.dimensions,
        }
    }

    /// Search for k nearest neighbors
    ///
    /// Uses HNSW O(log n) search if built, otherwise falls back to O(n) brute-force.
    fn search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        if self.pending.is_empty() {
            return Vec::new();
        }

        // Use HNSW search if index is built
        if self.is_built() {
            return self.hnsw_search(query, k);
        }

        // Fall back to brute-force search
        self.brute_force_search(query, k)
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
                        // Negate back to get actual dot product score
                        results.push((*candidate.value as i64, -candidate.distance));
                    }
                }
            }
        }

        results
    }

    /// Brute-force O(n) search fallback
    fn brute_force_search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        let mut results: Vec<(i64, f32)> = self
            .pending
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
}

impl VectorIndexManager {
    /// Create a new vector index manager
    pub fn new(config: VectorIndexConfig) -> Self {
        Self {
            config,
            indexes: RwLock::new(HashMap::new()),
            topic_configs: RwLock::new(HashMap::new()),
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

    /// Get or create an index for a topic-partition
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
            indexes.insert(key.clone(), PartitionIndex::new(config));
            debug!(
                "Created vector index for topic={}, partition={}",
                key.topic, key.partition
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
            "Added {} vectors to index for {}-{} (total: {})",
            added,
            topic,
            partition,
            index.pending.len()
        );

        Ok(added)
    }

    /// Search for k nearest neighbors in a topic-partition
    pub async fn search(
        &self,
        topic: &str,
        partition: i32,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(i64, f32)>> {
        let key = TopicPartitionKey::new(topic, partition);
        let indexes = self.indexes.read().await;

        let index = indexes
            .get(&key)
            .ok_or_else(|| anyhow!("No index for topic={}, partition={}", topic, partition))?;

        if query.len() != index.config.dimensions {
            return Err(anyhow!(
                "Query dimension mismatch: expected {}, got {}",
                index.config.dimensions,
                query.len()
            ));
        }

        Ok(index.search(query, k))
    }

    /// Search across all partitions of a topic
    pub async fn search_topic(
        &self,
        topic: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(i32, i64, f32)>> {
        let indexes = self.indexes.read().await;

        let mut all_results: Vec<(i32, i64, f32)> = Vec::new();

        for (key, index) in indexes.iter() {
            if key.topic == topic {
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

        if index.is_built() {
            return Ok(false);
        }

        index.build()?;
        info!(
            "Built HNSW index for {}-{} with {} vectors",
            topic, partition, index.pending.len()
        );
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
                if !index.is_built() {
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
                if !index.is_built() {
                    index.build()?;
                    built += 1;
                    debug!(
                        "Built HNSW index for {}-{} with {} vectors",
                        key.topic, key.partition, index.pending.len()
                    );
                }
            }
        }

        if built > 0 {
            info!("Built {} HNSW indexes across all topics", built);
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

    /// Get statistics for all partitions of a topic
    pub async fn get_topic_stats(&self, topic: &str) -> TopicIndexStats {
        let indexes = self.indexes.read().await;

        let mut stats = TopicIndexStats::default();

        for (key, index) in indexes.iter() {
            if key.topic == topic {
                let partition_stats = index.stats();
                stats.total_vectors += partition_stats.vector_count;
                stats.dimensions = partition_stats.dimensions;
                stats.partitions.insert(key.partition, partition_stats);
            }
        }

        stats.partition_count = stats.partitions.len();
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
    // Persistence Methods (Phase 5.3)
    // ========================================================================

    /// Save a single partition index to disk
    ///
    /// Serializes the index using bincode and writes to:
    /// `{base_path}/{topic}/{partition}.idx`
    pub async fn save_partition_index(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<()> {
        let key = TopicPartitionKey::new(topic, partition);

        // Get the index data
        let snapshot = {
            let indexes = self.indexes.read().await;
            let index = indexes
                .get(&key)
                .ok_or_else(|| anyhow!("No index for {}-{}", topic, partition))?;

            PartitionIndexSnapshot {
                config: index.config.clone(),
                vectors: index.pending.clone(),
                last_offset: index.last_offset,
            }
        };

        // Create directory structure
        let topic_dir = self.config.base_path.join(topic);
        fs::create_dir_all(&topic_dir).await?;

        // Serialize and write
        let path = topic_dir.join(format!("{}.idx", partition));
        let data = bincode::serialize(&snapshot)
            .map_err(|e| anyhow!("Failed to serialize index: {}", e))?;

        fs::write(&path, &data).await?;
        info!(
            topic = topic,
            partition = partition,
            vectors = snapshot.vectors.len(),
            path = %path.display(),
            "Saved partition index"
        );

        Ok(())
    }

    /// Load a single partition index from disk
    ///
    /// Loads from `{base_path}/{topic}/{partition}.idx`
    pub async fn load_partition_index(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<()> {
        let path = self.config.base_path.join(topic).join(format!("{}.idx", partition));

        if !path.exists() {
            return Err(anyhow!("Index file not found: {}", path.display()));
        }

        let data = fs::read(&path).await?;
        let snapshot: PartitionIndexSnapshot = bincode::deserialize(&data)
            .map_err(|e| anyhow!("Failed to deserialize index: {}", e))?;

        // Create the partition index
        let key = TopicPartitionKey::new(topic, partition);
        let vector_count = snapshot.vectors.len();
        let mut index = PartitionIndex::new(snapshot.config);
        index.pending = snapshot.vectors;
        index.last_offset = snapshot.last_offset;

        // Rebuild the HNSW index after loading vectors
        if vector_count > 0 {
            if let Err(e) = index.build() {
                warn!(
                    topic = topic,
                    partition = partition,
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
            vectors = vector_count,
            is_built = is_built,
            path = %path.display(),
            "Loaded partition index"
        );

        Ok(())
    }

    /// Save all indexes to disk
    ///
    /// Iterates through all indexes and saves each one.
    pub async fn save_all_indexes(&self) -> Result<usize> {
        let keys: Vec<TopicPartitionKey> = {
            let indexes = self.indexes.read().await;
            indexes.keys().cloned().collect()
        };

        let mut saved = 0;
        for key in keys {
            match self.save_partition_index(&key.topic, key.partition).await {
                Ok(_) => saved += 1,
                Err(e) => {
                    error!(
                        topic = %key.topic,
                        partition = key.partition,
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
    /// Scans the base path for index files and loads each one.
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
                    Some(name) => name,
                    None => continue,
                };

                // Parse partition number from filename
                if !file_name.ends_with(".idx") {
                    continue;
                }
                let partition: i32 = match file_name.trim_end_matches(".idx").parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                match self.load_partition_index(&topic, partition).await {
                    Ok(_) => loaded += 1,
                    Err(e) => {
                        warn!(
                            topic = %topic,
                            partition = partition,
                            error = %e,
                            "Failed to load partition index"
                        );
                    }
                }
            }
        }

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

    /// Get the path where an index would be stored
    pub fn index_path(&self, topic: &str, partition: i32) -> PathBuf {
        self.config.base_path.join(topic).join(format!("{}.idx", partition))
    }
}

/// Serializable snapshot of a partition index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionIndexSnapshot {
    /// HNSW configuration
    pub config: HnswIndexConfig,
    /// All vectors (id, vector)
    pub vectors: Vec<(u64, Vec<f32>)>,
    /// Last offset indexed
    pub last_offset: Option<i64>,
}

/// Embedding pipeline for batch processing messages
pub struct EmbeddingPipeline {
    /// Embedding provider
    provider: Arc<dyn EmbeddingProvider>,
    /// Vector index manager
    index_manager: Arc<VectorIndexManager>,
    /// Batch size for embedding requests
    batch_size: usize,
}

impl EmbeddingPipeline {
    /// Create a new embedding pipeline
    pub fn new(
        provider: Arc<dyn EmbeddingProvider>,
        index_manager: Arc<VectorIndexManager>,
        batch_size: usize,
    ) -> Self {
        Self {
            provider,
            index_manager,
            batch_size: batch_size.max(1).min(2048), // Clamp to reasonable range
        }
    }

    /// Process a batch of messages and add embeddings to the index
    pub async fn process_messages(
        &self,
        topic: &str,
        partition: i32,
        messages: Vec<(i64, String)>, // (offset, text)
    ) -> Result<ProcessingStats> {
        if messages.is_empty() {
            return Ok(ProcessingStats::default());
        }

        let mut stats = ProcessingStats {
            total_messages: messages.len(),
            ..Default::default()
        };

        // Process in batches
        for chunk in messages.chunks(self.batch_size) {
            let texts: Vec<&str> = chunk.iter().map(|(_, text)| text.as_str()).collect();
            let offsets: Vec<i64> = chunk.iter().map(|(offset, _)| *offset).collect();

            // v2.2.22: Track embedding latency and queue depth
            MetricsRecorder::increment_embedding_queue();
            let embed_start = Instant::now();

            match self.provider.embed_batch(&texts).await {
                Ok(batch) => {
                    // Record latency
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
                            text_preview: Some(result.text[..result.text.len().min(100)].to_string()),
                        })
                        .collect();

                    let added = self
                        .index_manager
                        .add_vectors(topic, partition, entries)
                        .await?;

                    stats.embedded += added;
                    stats.batches_processed += 1;
                    let tokens_used = batch.total_tokens.unwrap_or(0);
                    if tokens_used > 0 {
                        stats.total_tokens += tokens_used;
                    }

                    // Record success metrics
                    MetricsRecorder::record_embedding_request(
                        true,
                        chunk.len() as u64,
                        added as u64,
                        tokens_used as u64,
                    );
                }
                Err(e) => {
                    // Record latency even for errors
                    let latency = embed_start.elapsed();
                    MetricsRecorder::record_embedding_latency(latency);
                    MetricsRecorder::decrement_embedding_queue();

                    warn!(
                        "Embedding batch failed for {}-{}: {}",
                        topic, partition, e
                    );
                    stats.failed += chunk.len();
                    stats.errors.push(e.to_string());

                    // Record error metrics
                    MetricsRecorder::record_embedding_request(
                        false,
                        chunk.len() as u64,
                        0,
                        0,
                    );
                }
            }
        }

        info!(
            "Processed {} messages for {}-{}: {} embedded, {} failed",
            stats.total_messages, topic, partition, stats.embedded, stats.failed
        );

        Ok(stats)
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
    /// Embedding provider for query embedding
    provider: Arc<dyn EmbeddingProvider>,
}

impl VectorSearchService {
    /// Create a new vector search service
    pub fn new(
        index_manager: Arc<VectorIndexManager>,
        provider: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            index_manager,
            provider,
        }
    }

    /// Search by text query
    ///
    /// Embeds the query text using the configured provider, then searches
    /// the vector index for the k most similar messages.
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
        // Embed the query text
        let embedding_result = self.provider.embed(query_text).await?;
        let query_vector = embedding_result.vector;

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

    /// Get the embedding provider name
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// Get the embedding dimensions
    pub fn dimensions(&self) -> usize {
        self.provider.dimensions()
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
    fn test_hnsw_invalidation_on_add() {
        let config = HnswIndexConfig {
            dimensions: 2,
            metric: DistanceMetric::Euclidean,
            ..Default::default()
        };
        let mut index = PartitionIndex::new(config);

        index.add_vector(1, vec![1.0, 0.0]).unwrap();
        index.build().unwrap();
        assert!(index.is_built());

        // Adding a new vector should invalidate the built index
        index.add_vector(2, vec![0.0, 1.0]).unwrap();
        assert!(!index.is_built());

        // Rebuild
        index.build().unwrap();
        assert!(index.is_built());

        // Now both vectors should be searchable
        let results = index.search(&[0.5, 0.5], 2);
        assert_eq!(results.len(), 2);
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

        // Before build
        let stats = index.stats();
        assert_eq!(stats.vector_count, 2);
        assert_eq!(stats.pending_count, 2);
        assert!(!stats.is_built);

        // After build
        index.build().unwrap();
        let stats = index.stats();
        assert_eq!(stats.vector_count, 2);
        assert_eq!(stats.pending_count, 0);  // All vectors are in built index
        assert!(stats.is_built);
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
}
