//! In-memory vector index shadowing the WAL tail for near-real-time ANN search.
//!
//! See `docs/ROADMAP_HOT_PATH.md` (HP-2.1) for the full design.
//!
//! The hot vector path is purely in-memory. Storage is a linear `Vec<(offset,
//! vector)>` per `(topic, partition)`. For the expected Phase 2 working set
//! (≤ 50K vectors per partition) a brute-force top-k scan is faster to
//! maintain than an incrementally-built HNSW graph, and the latency fits
//! comfortably inside the overall freshness budget.
//!
//! Single-embed guarantee: the partition also stores a `HashMap<offset →
//! index into vectors>` so the WalIndexer can look up a cached embedding
//! before making a fresh embedding call when committing to cold HNSW.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use tokio::sync::RwLock;
use tracing::{trace, warn};

/// Distance metric for the hot vector index. Mirrors
/// `vector_index::DistanceMetric` but is re-declared locally so the hot path
/// crate has no cross-module dependency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HotDistanceMetric {
    Euclidean,
    Cosine,
    DotProduct,
}

impl HotDistanceMetric {
    #[inline]
    fn score(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            HotDistanceMetric::Euclidean => {
                // Return as distance (smaller = more similar); invert to score later.
                let sum: f32 = a
                    .iter()
                    .zip(b.iter())
                    .map(|(x, y)| (x - y).powi(2))
                    .sum();
                sum.sqrt()
            }
            HotDistanceMetric::Cosine => {
                let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                if na == 0.0 || nb == 0.0 {
                    1.0
                } else {
                    1.0 - (dot / (na * nb))
                }
            }
            HotDistanceMetric::DotProduct => {
                // Negate so lower == better (consistent with distance semantics).
                -a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>()
            }
        }
    }
}

/// A hit produced by searching the hot vector index.
#[derive(Debug, Clone)]
pub struct HotVectorHit {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    /// Lower = more similar. Convert to a score at the edge if needed.
    pub distance: f32,
}

/// Configuration for the hot vector index.
#[derive(Debug, Clone)]
pub struct HotVectorConfig {
    /// Soft cap on vectors per partition. When exceeded, oldest offsets are
    /// dropped on the next commit cycle.
    pub max_vectors_per_partition: usize,
    /// Expected vector dimensionality. Adds checked at insert time.
    pub dimensions: usize,
    /// Distance metric used by queries.
    pub metric: HotDistanceMetric,
}

impl Default for HotVectorConfig {
    fn default() -> Self {
        Self {
            max_vectors_per_partition: 50_000,
            dimensions: 1536,
            metric: HotDistanceMetric::Cosine,
        }
    }
}

/// Per-partition in-memory vector store.
struct HotVectorPartition {
    topic: String,
    partition: i32,
    config: HotVectorConfig,
    /// (offset, vector) pairs kept in insertion order.
    vectors: Vec<(i64, Vec<f32>)>,
    /// offset → index into `vectors` for O(1) lookup + dedup.
    offset_to_index: HashMap<i64, usize>,
    min_offset: Option<i64>,
    max_offset: Option<i64>,
}

impl HotVectorPartition {
    fn new(topic: String, partition: i32, config: HotVectorConfig) -> Self {
        Self {
            topic,
            partition,
            vectors: Vec::new(),
            offset_to_index: HashMap::new(),
            min_offset: None,
            max_offset: None,
            config,
        }
    }

    fn add(&mut self, offset: i64, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.config.dimensions {
            return Err(anyhow!(
                "hot vector dim mismatch for {}-{}: expected {}, got {}",
                self.topic,
                self.partition,
                self.config.dimensions,
                vector.len()
            ));
        }
        // Dedup — if we already have this offset, drop the duplicate.
        if self.offset_to_index.contains_key(&offset) {
            return Ok(());
        }
        let idx = self.vectors.len();
        self.vectors.push((offset, vector));
        self.offset_to_index.insert(offset, idx);
        self.min_offset = Some(self.min_offset.map_or(offset, |o| o.min(offset)));
        self.max_offset = Some(self.max_offset.map_or(offset, |o| o.max(offset)));
        Ok(())
    }

    fn get_cached(&self, offset: i64) -> Option<&[f32]> {
        self.offset_to_index
            .get(&offset)
            .and_then(|i| self.vectors.get(*i).map(|(_, v)| v.as_slice()))
    }

    /// Brute-force top-k search. At 50K vectors × 1536 dims this is ~10ms on
    /// a modern CPU — fine for the hot path's latency budget.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<HotVectorHit>> {
        if query.len() != self.config.dimensions {
            return Err(anyhow!(
                "query dim mismatch for {}-{}: expected {}, got {}",
                self.topic,
                self.partition,
                self.config.dimensions,
                query.len()
            ));
        }
        if k == 0 || self.vectors.is_empty() {
            return Ok(Vec::new());
        }
        let metric = self.config.metric;

        // Keep a top-k heap by distance (min-distance == best).
        let mut hits: Vec<HotVectorHit> = self
            .vectors
            .iter()
            .map(|(offset, v)| HotVectorHit {
                topic: self.topic.clone(),
                partition: self.partition,
                offset: *offset,
                distance: metric.score(query, v),
            })
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(k);
        Ok(hits)
    }

    /// Drop all vectors with offset ≤ threshold. Rebuilds the linear vec in
    /// place — O(n) but bounded by `max_vectors_per_partition`.
    fn evict_up_to(&mut self, threshold: i64) -> usize {
        let before = self.vectors.len();
        if before == 0 {
            return 0;
        }
        let mut new_vecs: Vec<(i64, Vec<f32>)> = Vec::with_capacity(before);
        let mut new_map: HashMap<i64, usize> = HashMap::with_capacity(before);
        let mut new_min: Option<i64> = None;
        let mut new_max: Option<i64> = None;
        for (offset, vec) in self.vectors.drain(..) {
            if offset <= threshold {
                continue;
            }
            new_map.insert(offset, new_vecs.len());
            new_min = Some(new_min.map_or(offset, |o| o.min(offset)));
            new_max = Some(new_max.map_or(offset, |o| o.max(offset)));
            new_vecs.push((offset, vec));
        }
        self.vectors = new_vecs;
        self.offset_to_index = new_map;
        self.min_offset = new_min;
        self.max_offset = new_max;
        before - self.vectors.len()
    }

    fn over_capacity(&self) -> bool {
        self.vectors.len() > self.config.max_vectors_per_partition
    }

    /// Keep only the most recent `max_vectors_per_partition` offsets.
    fn trim_to_capacity(&mut self) -> usize {
        if !self.over_capacity() {
            return 0;
        }
        let Some(max_off) = self.max_offset else {
            return 0;
        };
        let keep = self.config.max_vectors_per_partition as i64;
        let threshold = max_off.saturating_sub(keep);
        self.evict_up_to(threshold)
    }
}

/// Per-(topic, partition) in-memory vector store for near-real-time ANN.
pub struct HotVectorIndex {
    partitions: DashMap<(String, i32), Arc<RwLock<HotVectorPartition>>>,
    config: HotVectorConfig,
}

impl HotVectorIndex {
    pub fn new(config: HotVectorConfig) -> Self {
        Self {
            partitions: DashMap::new(),
            config,
        }
    }

    pub fn config(&self) -> &HotVectorConfig {
        &self.config
    }

    fn get_or_create(
        &self,
        topic: &str,
        partition: i32,
    ) -> Arc<RwLock<HotVectorPartition>> {
        if let Some(e) = self.partitions.get(&(topic.to_string(), partition)) {
            return e.clone();
        }
        let p = HotVectorPartition::new(topic.to_string(), partition, self.config.clone());
        let arc = Arc::new(RwLock::new(p));
        self.partitions
            .insert((topic.to_string(), partition), arc.clone());
        arc
    }

    pub async fn add_vector(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        vector: Vec<f32>,
    ) -> Result<()> {
        let part = self.get_or_create(topic, partition);
        let mut guard = part.write().await;
        guard.add(offset, vector)?;
        Ok(())
    }

    pub async fn add_batch(
        &self,
        topic: &str,
        partition: i32,
        entries: Vec<(i64, Vec<f32>)>,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let part = self.get_or_create(topic, partition);
        let mut guard = part.write().await;
        for (offset, vec) in entries {
            guard.add(offset, vec)?;
        }
        Ok(())
    }

    /// HP-2.6: Single-embed guarantee — look up a cached vector so WalIndexer
    /// can reuse it instead of re-calling the embedding provider. Returns
    /// `None` if the offset was never embedded via the hot path (dropped on
    /// overflow, embedder error, etc.); WalIndexer then embeds from scratch.
    pub async fn get_cached_vector(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> Option<Vec<f32>> {
        let entry = self.partitions.get(&(topic.to_string(), partition))?;
        let part = entry.clone();
        drop(entry);
        let guard = part.read().await;
        guard.get_cached(offset).map(|v| v.to_vec())
    }

    pub async fn search_partition(
        &self,
        topic: &str,
        partition: i32,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<HotVectorHit>> {
        let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
            return Ok(Vec::new());
        };
        let part = entry.clone();
        drop(entry);
        let guard = part.read().await;
        let start = std::time::Instant::now();
        let result = guard.search(query, k);
        chronik_monitoring::MetricsRecorder::record_hot_vector_search(start.elapsed());
        result
    }

    /// Search every partition of a topic and return the merged top-k.
    pub async fn search_topic(
        &self,
        topic: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<HotVectorHit>> {
        let keys: Vec<(String, i32)> = self
            .partitions
            .iter()
            .filter(|e| e.key().0 == topic)
            .map(|e| e.key().clone())
            .collect();
        let mut all = Vec::new();
        for (t, p) in keys {
            match self.search_partition(&t, p, query, k).await {
                Ok(mut hits) => all.append(&mut hits),
                Err(e) => trace!(%t, p, "hot vector search_partition error: {}", e),
            }
        }
        all.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all.truncate(k);
        Ok(all)
    }

    /// Drop offsets ≤ threshold for (topic, partition). Called by WalIndexer
    /// after cold HNSW commit via the `ColdFlushListener` wiring (HP-2.6).
    pub async fn evict_up_to(
        &self,
        topic: &str,
        partition: i32,
        threshold: i64,
    ) -> Result<usize> {
        let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
            return Ok(0);
        };
        let part = entry.clone();
        drop(entry);
        let mut guard = part.write().await;
        let evicted = guard.evict_up_to(threshold);
        if evicted > 0 {
            chronik_monitoring::MetricsRecorder::record_hot_vector_evict(evicted as u64);
        }
        Ok(evicted)
    }

    /// Background-timer hook: trim every partition to `max_vectors_per_partition`.
    /// Unlike the hot text path, the vector store has no hidden tombstones, so
    /// trimming alone is sufficient — no hard reset needed.
    pub async fn trim_all(&self) -> Result<()> {
        let keys: Vec<(String, i32)> = self
            .partitions
            .iter()
            .map(|e| e.key().clone())
            .collect();
        for (topic, partition) in keys {
            if let Some(entry) = self.partitions.get(&(topic.clone(), partition)) {
                let part = entry.clone();
                drop(entry);
                let mut guard = part.write().await;
                let trimmed = guard.trim_to_capacity();
                if trimmed > 0 {
                    trace!(%topic, partition, trimmed, "hot vector: trimmed on commit");
                }
            }
        }
        Ok(())
    }

    /// Spawn a detached trim timer. Returned JoinHandle can be aborted on
    /// shutdown but is usually left to the runtime.
    pub fn start_trim_timer(
        self: Arc<Self>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // skip the immediate tick
            loop {
                ticker.tick().await;
                if let Err(e) = self.trim_all().await {
                    warn!("hot vector trim_all failed: {}", e);
                }
            }
        })
    }

    pub async fn vector_count(&self, topic: &str, partition: i32) -> usize {
        let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
            return 0;
        };
        let part = entry.clone();
        drop(entry);
        let guard = part.read().await;
        guard.vectors.len()
    }

    pub fn partition_keys(&self) -> Vec<(String, i32)> {
        self.partitions.iter().map(|e| e.key().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dims: usize) -> HotVectorConfig {
        HotVectorConfig {
            max_vectors_per_partition: 100,
            dimensions: dims,
            metric: HotDistanceMetric::Cosine,
        }
    }

    fn v(values: &[f32]) -> Vec<f32> {
        values.to_vec()
    }

    #[tokio::test]
    async fn insert_and_search_single_partition() {
        let idx = HotVectorIndex::new(cfg(3));
        idx.add_vector("t", 0, 0, v(&[1.0, 0.0, 0.0])).await.unwrap();
        idx.add_vector("t", 0, 1, v(&[0.0, 1.0, 0.0])).await.unwrap();
        idx.add_vector("t", 0, 2, v(&[0.9, 0.1, 0.0])).await.unwrap();

        // Query close to offset 0.
        let hits = idx
            .search_partition("t", 0, &v(&[1.0, 0.0, 0.0]), 2)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].offset, 0);
        assert_eq!(hits[1].offset, 2);
    }

    #[tokio::test]
    async fn dedup_by_offset() {
        let idx = HotVectorIndex::new(cfg(2));
        idx.add_vector("t", 0, 0, v(&[1.0, 0.0])).await.unwrap();
        idx.add_vector("t", 0, 0, v(&[0.0, 1.0])).await.unwrap();
        assert_eq!(idx.vector_count("t", 0).await, 1);
    }

    #[tokio::test]
    async fn single_embed_cache_lookup() {
        let idx = HotVectorIndex::new(cfg(2));
        idx.add_vector("t", 0, 42, v(&[0.3, 0.4])).await.unwrap();
        let hit = idx.get_cached_vector("t", 0, 42).await;
        assert_eq!(hit, Some(vec![0.3, 0.4]));
        assert!(idx.get_cached_vector("t", 0, 43).await.is_none());
    }

    #[tokio::test]
    async fn evict_drops_old_offsets() {
        let idx = HotVectorIndex::new(cfg(2));
        for i in 0..10 {
            idx.add_vector("t", 0, i, v(&[i as f32, 0.0]))
                .await
                .unwrap();
        }
        assert_eq!(idx.vector_count("t", 0).await, 10);
        let evicted = idx.evict_up_to("t", 0, 4).await.unwrap();
        assert_eq!(evicted, 5);
        assert_eq!(idx.vector_count("t", 0).await, 5);
        assert!(idx.get_cached_vector("t", 0, 3).await.is_none());
        assert!(idx.get_cached_vector("t", 0, 5).await.is_some());
    }

    #[tokio::test]
    async fn trim_all_caps_vector_count() {
        let idx = HotVectorIndex::new(HotVectorConfig {
            max_vectors_per_partition: 5,
            dimensions: 2,
            metric: HotDistanceMetric::Cosine,
        });
        for i in 0..50 {
            idx.add_vector("t", 0, i, v(&[i as f32, 0.0]))
                .await
                .unwrap();
        }
        assert_eq!(idx.vector_count("t", 0).await, 50);
        idx.trim_all().await.unwrap();
        let remaining = idx.vector_count("t", 0).await;
        assert!(remaining <= 5, "expected ≤ 5, got {}", remaining);
    }

    #[tokio::test]
    async fn search_topic_merges_partitions() {
        let idx = HotVectorIndex::new(cfg(2));
        idx.add_vector("t", 0, 0, v(&[1.0, 0.0])).await.unwrap();
        idx.add_vector("t", 1, 0, v(&[0.99, 0.0])).await.unwrap();
        idx.add_vector("t", 2, 0, v(&[0.0, 1.0])).await.unwrap();

        let hits = idx.search_topic("t", &v(&[1.0, 0.0]), 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        // The two closest should be partition 0 offset 0 and partition 1 offset 0.
        let parts: std::collections::HashSet<i32> = hits.iter().map(|h| h.partition).collect();
        assert!(parts.contains(&0));
        assert!(parts.contains(&1));
    }

    #[tokio::test]
    async fn dim_mismatch_is_rejected() {
        let idx = HotVectorIndex::new(cfg(3));
        let err = idx.add_vector("t", 0, 0, v(&[1.0, 0.0])).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn missing_partition_returns_empty() {
        let idx = HotVectorIndex::new(cfg(2));
        let hits = idx
            .search_partition("nope", 0, &v(&[1.0, 0.0]), 5)
            .await
            .unwrap();
        assert!(hits.is_empty());
        assert_eq!(idx.evict_up_to("nope", 0, 10).await.unwrap(), 0);
        assert!(idx.get_cached_vector("nope", 0, 0).await.is_none());
    }
}
