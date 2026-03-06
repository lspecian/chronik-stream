//! Query embedding cache for vector search.
//!
//! Caches query text → embedding vector mappings to avoid repeated calls to the
//! embedding API (e.g., OpenAI). Reduces vector query latency from ~372ms to ~1-5ms
//! for repeated queries.
//!
//! ## Configuration
//!
//! | Env Var | Default | Description |
//! |---------|---------|-------------|
//! | `CHRONIK_EMBEDDING_CACHE_ENABLED` | `true` | Enable/disable cache |
//! | `CHRONIK_EMBEDDING_CACHE_MAX_ENTRIES` | `10000` | Max cached embeddings |
//! | `CHRONIK_EMBEDDING_CACHE_TTL_SECS` | `3600` | TTL before eviction |

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Cached embedding entry.
struct CachedEmbedding {
    vector: Vec<f32>,
    created_at: Instant,
    hit_count: u64,
}

/// LRU-style cache for query embeddings with TTL expiry.
///
/// Thread-safe via `tokio::sync::RwLock`. Reads are concurrent; writes
/// (cache miss → insert) are serialized but infrequent.
pub struct QueryEmbeddingCache {
    cache: RwLock<HashMap<String, CachedEmbedding>>,
    max_entries: usize,
    ttl: Duration,
    /// Dimensions per vector (set on first insert, used for memory estimation)
    dimensions: RwLock<usize>,
}

impl QueryEmbeddingCache {
    /// Create a new cache with the given capacity and TTL.
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            cache: RwLock::new(HashMap::with_capacity(max_entries.min(1024))),
            max_entries,
            ttl,
            dimensions: RwLock::new(0),
        }
    }

    /// Look up a cached embedding by normalized key.
    ///
    /// Returns `None` if not found or if the entry has expired (expired entries
    /// are lazily removed on the next write).
    pub async fn get(&self, key: &str) -> Option<Vec<f32>> {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.get_mut(key) {
            if entry.created_at.elapsed() < self.ttl {
                entry.hit_count += 1;
                return Some(entry.vector.clone());
            }
            // Expired — remove it
            cache.remove(key);
        }
        None
    }

    /// Insert an embedding into the cache.
    ///
    /// If at capacity, evicts the oldest entry (by `created_at`).
    pub async fn insert(&self, key: String, vector: Vec<f32>) {
        let mut cache = self.cache.write().await;

        // Update dimensions on first insert
        {
            let mut dims = self.dimensions.write().await;
            if *dims == 0 && !vector.is_empty() {
                *dims = vector.len();
            }
        }

        // Evict oldest if at capacity
        if cache.len() >= self.max_entries && !cache.contains_key(&key) {
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, v)| v.created_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
                chronik_monitoring::MetricsRecorder::record_embedding_cache_operation("eviction");
            }
        }

        cache.insert(
            key,
            CachedEmbedding {
                vector,
                created_at: Instant::now(),
                hit_count: 0,
            },
        );
    }

    /// Current number of cached entries.
    pub async fn size(&self) -> usize {
        self.cache.read().await.len()
    }

    /// Estimated memory usage in bytes.
    ///
    /// Approximation: entries × dimensions × 4 bytes per f32 + key overhead.
    pub async fn memory_bytes(&self) -> usize {
        let cache = self.cache.read().await;
        let dims = *self.dimensions.read().await;
        let per_entry = dims * 4 + 64; // vector bytes + key/metadata overhead
        cache.len() * per_entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cache_hit_miss() {
        let cache = QueryEmbeddingCache::new(100, Duration::from_secs(60));

        // Miss
        assert!(cache.get("hello").await.is_none());

        // Insert
        cache.insert("hello".to_string(), vec![1.0, 2.0, 3.0]).await;

        // Hit
        let result = cache.get("hello").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap(), vec![1.0, 2.0, 3.0]);

        // Different key — miss
        assert!(cache.get("world").await.is_none());
    }

    #[tokio::test]
    async fn test_cache_ttl_expiry() {
        let cache = QueryEmbeddingCache::new(100, Duration::from_millis(50));

        cache.insert("key".to_string(), vec![1.0]).await;
        assert!(cache.get("key").await.is_some());

        // Wait past TTL
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cache.get("key").await.is_none());
    }

    #[tokio::test]
    async fn test_cache_max_entries_eviction() {
        let cache = QueryEmbeddingCache::new(3, Duration::from_secs(60));

        cache.insert("a".to_string(), vec![1.0]).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.insert("b".to_string(), vec![2.0]).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.insert("c".to_string(), vec![3.0]).await;

        assert_eq!(cache.size().await, 3);

        // Insert 4th — should evict "a" (oldest)
        cache.insert("d".to_string(), vec![4.0]).await;
        assert_eq!(cache.size().await, 3);
        assert!(cache.get("a").await.is_none()); // evicted
        assert!(cache.get("d").await.is_some()); // present
    }

    #[tokio::test]
    async fn test_cache_key_normalization() {
        let cache = QueryEmbeddingCache::new(100, Duration::from_secs(60));

        // Callers are responsible for normalization, but test that same key hits
        let key = "Hello World".trim().to_lowercase();
        cache.insert(key.clone(), vec![1.0, 2.0]).await;

        let key2 = "  hello world  ".trim().to_lowercase();
        let result = cache.get(&key2).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap(), vec![1.0, 2.0]);
    }

    #[tokio::test]
    async fn test_cache_memory_estimate() {
        let cache = QueryEmbeddingCache::new(100, Duration::from_secs(60));

        // Empty cache
        assert_eq!(cache.memory_bytes().await, 0);

        // Insert 1536-dimensional vector (OpenAI text-embedding-3-small)
        cache
            .insert("test".to_string(), vec![0.1; 1536])
            .await;

        let mem = cache.memory_bytes().await;
        // 1 entry × (1536 × 4 + 64) = 6208 bytes
        assert!(mem > 6000);
        assert!(mem < 7000);
    }

    #[tokio::test]
    async fn test_cache_overwrite_existing_key() {
        let cache = QueryEmbeddingCache::new(3, Duration::from_secs(60));

        cache.insert("a".to_string(), vec![1.0]).await;
        cache.insert("b".to_string(), vec![2.0]).await;
        cache.insert("c".to_string(), vec![3.0]).await;

        // Overwrite "a" — should NOT evict since key already exists
        cache.insert("a".to_string(), vec![10.0]).await;
        assert_eq!(cache.size().await, 3);
        assert_eq!(cache.get("a").await.unwrap(), vec![10.0]);
    }
}
