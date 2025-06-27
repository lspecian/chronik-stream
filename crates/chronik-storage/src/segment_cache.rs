//! Caching layer for segment reader to improve performance.
//! 
//! Provides LRU cache for frequently accessed segments with:
//! - Configurable cache size limits
//! - TTL-based expiration
//! - Cache warming strategies
//! - Metrics and monitoring

use crate::{ChronikSegment, SegmentReader, FetchResult, RecordBatch};
use chronik_common::{Result, Error};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Cache entry for a segment
#[derive(Clone)]
struct CacheEntry {
    segment: Arc<ChronikSegment>,
    last_access: Instant,
    access_count: u64,
    size_bytes: usize,
}

/// Cache statistics
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
    pub size_bytes: usize,
}

impl CacheStats {
    /// Calculate hit rate percentage
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total > 0 {
            (self.hits as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }
}

/// Configuration for segment cache
#[derive(Debug, Clone)]
pub struct SegmentCacheConfig {
    /// Maximum number of segments to cache
    pub max_entries: usize,
    /// Maximum total size of cached segments in bytes
    pub max_size_bytes: usize,
    /// TTL for cache entries
    pub ttl: Duration,
    /// Whether to warm cache on startup
    pub warm_on_startup: bool,
    /// Eviction strategy
    pub eviction_policy: EvictionPolicy,
}

impl Default for SegmentCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 100,
            max_size_bytes: 1024 * 1024 * 1024, // 1GB
            ttl: Duration::from_secs(3600), // 1 hour
            warm_on_startup: false,
            eviction_policy: EvictionPolicy::Lru,
        }
    }
}

/// Cache eviction policy
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EvictionPolicy {
    /// Least Recently Used
    Lru,
    /// Least Frequently Used
    Lfu,
    /// Time-based TTL only
    Ttl,
}

/// Segment cache with LRU eviction
pub struct SegmentCache {
    config: SegmentCacheConfig,
    cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
    stats: Arc<RwLock<CacheStats>>,
    reader: Arc<SegmentReader>,
}

impl SegmentCache {
    /// Create new segment cache
    pub fn new(config: SegmentCacheConfig, reader: Arc<SegmentReader>) -> Self {
        Self {
            config,
            cache: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(CacheStats::default())),
            reader,
        }
    }
    
    /// Get segment from cache or load from storage
    pub async fn get_segment(&self, key: &str) -> Result<Arc<ChronikSegment>> {
        // Check cache first
        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(key) {
                // Check TTL
                if entry.last_access.elapsed() < self.config.ttl {
                    entry.last_access = Instant::now();
                    entry.access_count += 1;
                    
                    let mut stats = self.stats.write();
                    stats.hits += 1;
                    
                    debug!("Cache hit for segment: {}", key);
                    return Ok(entry.segment.clone());
                } else {
                    // Entry expired
                    cache.remove(key);
                    let mut stats = self.stats.write();
                    stats.evictions += 1;
                    stats.entries = cache.len();
                }
            }
        }
        
        // Cache miss - load from storage
        let mut stats = self.stats.write();
        stats.misses += 1;
        drop(stats);
        
        debug!("Cache miss for segment: {}, loading from storage", key);
        
        // Parse segment location from key
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() < 3 {
            return Err(Error::InvalidSegment(format!("Invalid segment key: {}", key)));
        }
        
        let topic = parts[0];
        let partition: i32 = parts[1].parse()
            .map_err(|_| Error::InvalidSegment(format!("Invalid partition in key: {}", key)))?;
        let filename = parts[2];
        
        // Load segment using reader
        // Build segment path - we'll need to get this from the reader's method
        // For now, use a standard path structure
        let segment_path = PathBuf::from(format!("data/{}/{}/{}", topic, partition, filename));
        let segment = self.load_segment_from_file(&segment_path).await?;
        let segment_arc = Arc::new(segment);
        
        // Calculate segment size (approximate)
        let size_bytes = self.estimate_segment_size(&segment_arc);
        
        // Add to cache with eviction if needed
        self.add_to_cache(key.to_string(), segment_arc.clone(), size_bytes)?;
        
        Ok(segment_arc)
    }
    
    /// Load segment from file
    async fn load_segment_from_file(&self, path: &Path) -> Result<ChronikSegment> {
        use tokio::fs::File;
        use tokio::io::AsyncReadExt;
        
        let mut file = File::open(path).await
            .map_err(|e| Error::Io(e))?;
        
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).await
            .map_err(|e| Error::Io(e))?;
        
        let mut cursor = std::io::Cursor::new(buffer);
        ChronikSegment::read_from(&mut cursor)
    }
    
    /// Estimate segment size in memory
    fn estimate_segment_size(&self, segment: &ChronikSegment) -> usize {
        // Base size
        let mut size = std::mem::size_of::<ChronikSegment>();
        
        // Add metadata size
        size += std::mem::size_of_val(segment.metadata());
        
        // Add kafka data size
        size += segment.kafka_data().iter()
            .map(|batch| {
                std::mem::size_of::<RecordBatch>() + 
                batch.records.iter()
                    .map(|r| {
                        r.key.as_ref().map(|k| k.len()).unwrap_or(0) +
                        r.value.len() +
                        r.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
                    })
                    .sum::<usize>()
            })
            .sum::<usize>();
        
        // Add total uncompressed size from metadata
        size += segment.metadata().total_uncompressed_size;
        
        size
    }
    
    /// Add segment to cache with eviction
    fn add_to_cache(&self, key: String, segment: Arc<ChronikSegment>, size_bytes: usize) -> Result<()> {
        let mut cache = self.cache.write();
        let mut stats = self.stats.write();
        
        // Check if we need to evict entries
        while self.should_evict(&cache, size_bytes) {
            if let Some(evict_key) = self.select_eviction_candidate(&cache) {
                if let Some(entry) = cache.remove(&evict_key) {
                    stats.size_bytes = stats.size_bytes.saturating_sub(entry.size_bytes);
                    stats.evictions += 1;
                    debug!("Evicted segment from cache: {}", evict_key);
                }
            } else {
                break;
            }
        }
        
        // Add new entry
        let entry = CacheEntry {
            segment,
            last_access: Instant::now(),
            access_count: 1,
            size_bytes,
        };
        
        cache.insert(key.clone(), entry);
        stats.entries = cache.len();
        stats.size_bytes += size_bytes;
        
        info!("Added segment to cache: {} (size: {} bytes, total: {} entries, {} MB)", 
              key, size_bytes, stats.entries, stats.size_bytes / 1024 / 1024);
        
        Ok(())
    }
    
    /// Check if eviction is needed
    fn should_evict(&self, cache: &HashMap<String, CacheEntry>, new_size: usize) -> bool {
        let stats = self.stats.read();
        
        cache.len() >= self.config.max_entries ||
        stats.size_bytes + new_size > self.config.max_size_bytes
    }
    
    /// Select entry to evict based on policy
    fn select_eviction_candidate(&self, cache: &HashMap<String, CacheEntry>) -> Option<String> {
        match self.config.eviction_policy {
            EvictionPolicy::Lru => {
                // Find least recently used
                cache.iter()
                    .min_by_key(|(_, entry)| entry.last_access)
                    .map(|(key, _)| key.clone())
            }
            EvictionPolicy::Lfu => {
                // Find least frequently used
                cache.iter()
                    .min_by_key(|(_, entry)| entry.access_count)
                    .map(|(key, _)| key.clone())
            }
            EvictionPolicy::Ttl => {
                // Find expired entries
                let now = Instant::now();
                cache.iter()
                    .find(|(_, entry)| now.duration_since(entry.last_access) > self.config.ttl)
                    .map(|(key, _)| key.clone())
            }
        }
    }
    
    /// Warm cache by preloading segments
    pub async fn warm_cache(&self, segment_keys: Vec<String>) -> Result<()> {
        info!("Warming cache with {} segments", segment_keys.len());
        
        for key in segment_keys {
            match self.get_segment(&key).await {
                Ok(_) => debug!("Warmed cache with segment: {}", key),
                Err(e) => warn!("Failed to warm cache with segment {}: {}", key, e),
            }
        }
        
        let stats = self.stats.read();
        info!("Cache warming complete: {} entries, {} MB", 
              stats.entries, stats.size_bytes / 1024 / 1024);
        
        Ok(())
    }
    
    /// Clear all cache entries
    pub fn clear(&self) {
        let mut cache = self.cache.write();
        let mut stats = self.stats.write();
        
        cache.clear();
        *stats = CacheStats::default();
        
        info!("Cache cleared");
    }
    
    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        self.stats.read().clone()
    }
    
    /// Evict expired entries
    pub fn evict_expired(&self) -> usize {
        let mut cache = self.cache.write();
        let mut stats = self.stats.write();
        
        let now = Instant::now();
        let expired_keys: Vec<String> = cache.iter()
            .filter(|(_, entry)| now.duration_since(entry.last_access) > self.config.ttl)
            .map(|(key, _)| key.clone())
            .collect();
        
        let evicted_count = expired_keys.len();
        
        for key in expired_keys {
            if let Some(entry) = cache.remove(&key) {
                stats.size_bytes = stats.size_bytes.saturating_sub(entry.size_bytes);
                stats.evictions += 1;
            }
        }
        
        stats.entries = cache.len();
        
        if evicted_count > 0 {
            info!("Evicted {} expired entries from cache", evicted_count);
        }
        
        evicted_count
    }
    
    /// Start background eviction task
    pub fn start_eviction_task(self: Arc<Self>) {
        let cache = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            
            loop {
                interval.tick().await;
                cache.evict_expired();
            }
        });
    }
}

/// Cached segment reader wrapping the regular reader
pub struct CachedSegmentReader {
    cache: Arc<SegmentCache>,
    reader: Arc<SegmentReader>,
}

impl CachedSegmentReader {
    /// Create new cached reader
    pub fn new(config: SegmentCacheConfig, reader: Arc<SegmentReader>) -> Self {
        let cache = Arc::new(SegmentCache::new(config, reader.clone()));
        
        // Start background eviction task
        cache.clone().start_eviction_task();
        
        Self { cache, reader }
    }
    
    /// Get cache reference
    pub fn cache(&self) -> &Arc<SegmentCache> {
        &self.cache
    }
    
    /// Fetch records with caching
    pub async fn fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: i32,
        _max_wait_ms: i32,
    ) -> Result<FetchResult> {
        // Delegate to underlying reader - caching happens at segment level
        self.reader.fetch(topic, partition, offset, max_bytes).await
    }
    
    /// Read segment with caching
    pub async fn read_segment(
        &self,
        topic: &str,
        partition: i32,
        segment_file: &str,
    ) -> Result<Arc<ChronikSegment>> {
        let key = format!("{}/{}/{}", topic, partition, segment_file);
        self.cache.get_segment(&key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    fn create_test_config() -> SegmentCacheConfig {
        SegmentCacheConfig {
            max_entries: 10,
            max_size_bytes: 1024 * 1024, // 1MB
            ttl: Duration::from_secs(60),
            warm_on_startup: false,
            eviction_policy: EvictionPolicy::Lru,
        }
    }
    
    #[tokio::test]
    async fn test_cache_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let reader_config = SegmentReaderConfig {
            data_dir: temp_dir.path().to_path_buf(),
            cache_size_mb: 100,
            prefetch_size: 10,
        };
        
        let reader = Arc::new(SegmentReader::new(reader_config));
        let cache = SegmentCache::new(create_test_config(), reader);
        
        // Test cache miss
        let result = cache.get_segment("test/0/segment1.chronik").await;
        assert!(result.is_err()); // File doesn't exist
        
        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);
    }
    
    #[test]
    fn test_cache_stats() {
        let stats = CacheStats {
            hits: 75,
            misses: 25,
            evictions: 5,
            entries: 10,
            size_bytes: 1024 * 1024,
        };
        
        assert_eq!(stats.hit_rate(), 75.0);
    }
    
    #[test]
    fn test_eviction_policy() {
        let config = create_test_config();
        assert_eq!(config.eviction_policy, EvictionPolicy::Lru);
    }
}