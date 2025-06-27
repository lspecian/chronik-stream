//! Query result caching for improved performance

use crate::api::{SearchRequest, SearchResponse};
use chronik_common::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use sha2::{Sha256, Digest};

/// Cache configuration
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of cached entries
    pub max_entries: usize,
    /// Time-to-live for cache entries
    pub ttl: Duration,
    /// Whether caching is enabled
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 10000,
            ttl: Duration::from_secs(300), // 5 minutes
            enabled: true,
        }
    }
}

/// Cache entry
#[derive(Debug, Clone)]
struct CacheEntry {
    /// Cached response
    response: SearchResponse,
    /// Timestamp when entry was created
    created_at: SystemTime,
    /// Number of times this entry was accessed
    hit_count: u64,
}

/// Query result cache
pub struct QueryCache {
    /// Cache storage
    cache: Arc<DashMap<String, CacheEntry>>,
    /// Cache configuration
    config: CacheConfig,
    /// Cache statistics
    stats: Arc<DashMap<&'static str, u64>>,
}

impl QueryCache {
    /// Create a new query cache
    pub fn new(config: CacheConfig) -> Self {
        let stats = Arc::new(DashMap::new());
        stats.insert("hits", 0);
        stats.insert("misses", 0);
        stats.insert("evictions", 0);
        
        Self {
            cache: Arc::new(DashMap::new()),
            config,
            stats,
        }
    }
    
    /// Generate cache key from request
    pub fn generate_key(index: Option<&str>, request: &SearchRequest) -> String {
        let mut hasher = Sha256::new();
        
        // Include index in key
        if let Some(idx) = index {
            hasher.update(idx.as_bytes());
        }
        
        // Serialize request to generate key
        if let Ok(serialized) = serde_json::to_vec(request) {
            hasher.update(&serialized);
        }
        
        let result = hasher.finalize();
        hex::encode(result)
    }
    
    /// Get cached response
    pub fn get(&self, key: &str) -> Option<SearchResponse> {
        if !self.config.enabled {
            return None;
        }
        
        if let Some(mut entry) = self.cache.get_mut(key) {
            let now = SystemTime::now();
            let age = now.duration_since(entry.created_at).unwrap_or(Duration::from_secs(0));
            
            if age <= self.config.ttl {
                // Update hit count
                entry.hit_count += 1;
                
                // Update stats
                if let Some(mut hits) = self.stats.get_mut("hits") {
                    *hits += 1;
                }
                
                return Some(entry.response.clone());
            } else {
                // Entry expired, remove it
                drop(entry);
                self.cache.remove(key);
                
                // Update stats
                if let Some(mut evictions) = self.stats.get_mut("evictions") {
                    *evictions += 1;
                }
            }
        }
        
        // Update stats
        if let Some(mut misses) = self.stats.get_mut("misses") {
            *misses += 1;
        }
        
        None
    }
    
    /// Put response in cache
    pub fn put(&self, key: String, response: SearchResponse) {
        if !self.config.enabled {
            return;
        }
        
        // Check if we need to evict entries
        if self.cache.len() >= self.config.max_entries {
            self.evict_oldest();
        }
        
        let entry = CacheEntry {
            response,
            created_at: SystemTime::now(),
            hit_count: 0,
        };
        
        self.cache.insert(key, entry);
    }
    
    /// Evict oldest entries to make room
    fn evict_oldest(&self) {
        let mut oldest_key = None;
        let mut oldest_time = SystemTime::now();
        
        // Find oldest entry
        for entry in self.cache.iter() {
            if entry.value().created_at < oldest_time {
                oldest_time = entry.value().created_at;
                oldest_key = Some(entry.key().clone());
            }
        }
        
        // Remove oldest
        if let Some(key) = oldest_key {
            self.cache.remove(&key);
            
            // Update stats
            if let Some(mut evictions) = self.stats.get_mut("evictions") {
                *evictions += 1;
            }
        }
    }
    
    /// Clear the cache
    pub fn clear(&self) {
        self.cache.clear();
    }
    
    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let hits = self.stats.get("hits").map(|v| *v).unwrap_or(0);
        let misses = self.stats.get("misses").map(|v| *v).unwrap_or(0);
        let evictions = self.stats.get("evictions").map(|v| *v).unwrap_or(0);
        
        let total_requests = hits + misses;
        let hit_rate = if total_requests > 0 {
            (hits as f64 / total_requests as f64) * 100.0
        } else {
            0.0
        };
        
        CacheStats {
            entries: self.cache.len(),
            hits,
            misses,
            evictions,
            hit_rate,
        }
    }
}

/// Cache statistics
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    /// Number of entries in cache
    pub entries: usize,
    /// Number of cache hits
    pub hits: u64,
    /// Number of cache misses
    pub misses: u64,
    /// Number of evictions
    pub evictions: u64,
    /// Hit rate percentage
    pub hit_rate: f64,
}

/// Hex encoding utility
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref()
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_cache_key_generation() {
        let request = SearchRequest {
            query: None,
            size: 10,
            from: 0,
            sort: None,
            _source: None,
            aggs: None,
            aggregations: None,
            highlight: None,
        };
        
        let key1 = QueryCache::generate_key(Some("index1"), &request);
        let key2 = QueryCache::generate_key(Some("index2"), &request);
        let key3 = QueryCache::generate_key(Some("index1"), &request);
        
        assert_ne!(key1, key2);
        assert_eq!(key1, key3);
    }
    
    #[test]
    fn test_cache_operations() {
        let config = CacheConfig {
            max_entries: 2,
            ttl: Duration::from_secs(60),
            enabled: true,
        };
        
        let cache = QueryCache::new(config);
        
        // Test put and get
        let response = SearchResponse {
            took: 10,
            timed_out: false,
            _shards: crate::api::ShardInfo {
                total: 1,
                successful: 1,
                skipped: 0,
                failed: 0,
            },
            hits: crate::api::HitsInfo {
                total: crate::api::TotalHits {
                    value: 0,
                    relation: "eq".to_string(),
                },
                max_score: None,
                hits: vec![],
            },
            aggregations: None,
        };
        
        cache.put("key1".to_string(), response.clone());
        assert!(cache.get("key1").is_some());
        assert!(cache.get("key2").is_none());
        
        // Test eviction
        cache.put("key2".to_string(), response.clone());
        cache.put("key3".to_string(), response.clone());
        
        // key1 should have been evicted
        assert!(cache.get("key1").is_none());
        assert!(cache.get("key2").is_some());
        assert!(cache.get("key3").is_some());
    }
}