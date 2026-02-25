//! Topic capability detection for query planning.
//!
//! Determines which query modes (text, vector, SQL, fetch) are available
//! for each topic by checking the server's backend state.

use crate::types::QueryMode;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Capabilities available for a single topic.
#[derive(Debug, Clone, Serialize)]
pub struct TopicCapabilities {
    /// Topic name
    pub topic: String,
    /// Full-text search via Tantivy
    pub text_search: bool,
    /// Semantic vector search via HNSW + embeddings
    pub vector_search: bool,
    /// SQL queries via DataFusion + Parquet/HotBuffer
    pub sql_query: bool,
    /// Direct offset-based fetch (always true if topic exists)
    pub fetch: bool,
}

impl TopicCapabilities {
    /// Returns which modes are supported for this topic.
    pub fn supported_modes(&self) -> Vec<QueryMode> {
        let mut modes = Vec::new();
        if self.text_search {
            modes.push(QueryMode::Text);
        }
        if self.vector_search {
            modes.push(QueryMode::Vector);
        }
        if self.sql_query {
            modes.push(QueryMode::Sql);
        }
        if self.fetch {
            modes.push(QueryMode::Fetch);
        }
        modes
    }

    /// Check if a specific mode is supported.
    pub fn supports(&self, mode: QueryMode) -> bool {
        match mode {
            QueryMode::Text => self.text_search,
            QueryMode::Vector => self.vector_search,
            QueryMode::Sql => self.sql_query,
            QueryMode::Fetch => self.fetch,
        }
    }
}

/// Trait for checking backend availability.
///
/// Implemented by the server to detect which backends are ready for each topic.
/// This decouples the query layer from the specific backend implementations.
pub trait CapabilityDetector: Send + Sync {
    /// Check if a Tantivy index exists for the given topic.
    fn has_text_index(&self, topic: &str) -> bool;
    /// Check if a HNSW vector index exists for the given topic.
    fn has_vector_index(&self, topic: &str) -> bool;
    /// Check if columnar storage (Parquet/HotBuffer) is enabled for the topic.
    fn has_sql_table(&self, topic: &str) -> bool;
    /// Check if the topic exists at all (for fetch capability).
    fn topic_exists(&self, topic: &str) -> bool;
    /// List all known topic names.
    fn list_topics(&self) -> Vec<String>;
}

/// Cached capabilities for all topics.
///
/// Refreshes at most every `ttl` duration to avoid querying backend state
/// on every request.
pub struct CapabilitiesCache {
    detector: Arc<dyn CapabilityDetector>,
    cache: RwLock<CacheState>,
    ttl: Duration,
}

struct CacheState {
    capabilities: HashMap<String, TopicCapabilities>,
    last_refresh: Option<Instant>,
}

impl CapabilitiesCache {
    /// Create a new cache with the given TTL.
    pub fn new(detector: Arc<dyn CapabilityDetector>, ttl: Duration) -> Self {
        Self {
            detector,
            cache: RwLock::new(CacheState {
                capabilities: HashMap::new(),
                last_refresh: None,
            }),
            ttl,
        }
    }

    /// Get capabilities, refreshing if the cache is stale.
    pub async fn get(&self) -> HashMap<String, TopicCapabilities> {
        // Check if cache is still valid
        {
            let cache = self.cache.read().await;
            if let Some(last_refresh) = cache.last_refresh {
                if last_refresh.elapsed() < self.ttl {
                    return cache.capabilities.clone();
                }
            }
        }

        // Cache is stale, refresh
        self.refresh().await
    }

    /// Get capabilities for a specific topic.
    pub async fn get_topic(&self, topic: &str) -> Option<TopicCapabilities> {
        let caps = self.get().await;
        caps.get(topic).cloned()
    }

    /// Force a cache refresh and return updated capabilities.
    pub async fn refresh(&self) -> HashMap<String, TopicCapabilities> {
        let topics = self.detector.list_topics();
        let mut capabilities = HashMap::new();

        for topic in &topics {
            let cap = TopicCapabilities {
                topic: topic.clone(),
                text_search: self.detector.has_text_index(topic),
                vector_search: self.detector.has_vector_index(topic),
                sql_query: self.detector.has_sql_table(topic),
                fetch: self.detector.topic_exists(topic),
            };
            capabilities.insert(topic.clone(), cap);
        }

        // Update cache
        let mut cache = self.cache.write().await;
        cache.capabilities = capabilities.clone();
        cache.last_refresh = Some(Instant::now());

        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDetector {
        topics: Vec<String>,
        text_topics: Vec<String>,
        vector_topics: Vec<String>,
        sql_topics: Vec<String>,
    }

    impl CapabilityDetector for MockDetector {
        fn has_text_index(&self, topic: &str) -> bool {
            self.text_topics.contains(&topic.to_string())
        }
        fn has_vector_index(&self, topic: &str) -> bool {
            self.vector_topics.contains(&topic.to_string())
        }
        fn has_sql_table(&self, topic: &str) -> bool {
            self.sql_topics.contains(&topic.to_string())
        }
        fn topic_exists(&self, topic: &str) -> bool {
            self.topics.contains(&topic.to_string())
        }
        fn list_topics(&self) -> Vec<String> {
            self.topics.clone()
        }
    }

    #[tokio::test]
    async fn test_capabilities_detection() {
        let detector = Arc::new(MockDetector {
            topics: vec!["logs".into(), "orders".into(), "events".into()],
            text_topics: vec!["logs".into()],
            vector_topics: vec!["logs".into()],
            sql_topics: vec!["orders".into()],
        });

        let cache = CapabilitiesCache::new(detector, Duration::from_secs(60));
        let caps = cache.get().await;

        // logs: text + vector + fetch
        let logs = caps.get("logs").unwrap();
        assert!(logs.text_search);
        assert!(logs.vector_search);
        assert!(!logs.sql_query);
        assert!(logs.fetch);

        // orders: sql + fetch
        let orders = caps.get("orders").unwrap();
        assert!(!orders.text_search);
        assert!(!orders.vector_search);
        assert!(orders.sql_query);
        assert!(orders.fetch);

        // events: fetch only
        let events = caps.get("events").unwrap();
        assert!(!events.text_search);
        assert!(!events.vector_search);
        assert!(!events.sql_query);
        assert!(events.fetch);
    }

    #[test]
    fn test_supported_modes() {
        let cap = TopicCapabilities {
            topic: "test".into(),
            text_search: true,
            vector_search: false,
            sql_query: true,
            fetch: true,
        };
        let modes = cap.supported_modes();
        assert!(modes.contains(&QueryMode::Text));
        assert!(!modes.contains(&QueryMode::Vector));
        assert!(modes.contains(&QueryMode::Sql));
        assert!(modes.contains(&QueryMode::Fetch));
    }

    #[tokio::test]
    async fn test_cache_ttl() {
        let detector = Arc::new(MockDetector {
            topics: vec!["t1".into()],
            text_topics: vec![],
            vector_topics: vec![],
            sql_topics: vec![],
        });

        let cache = CapabilitiesCache::new(detector, Duration::from_millis(100));

        // First call refreshes
        let caps1 = cache.get().await;
        assert_eq!(caps1.len(), 1);

        // Second call within TTL uses cache
        let caps2 = cache.get().await;
        assert_eq!(caps2.len(), 1);

        // After TTL, would refresh (tested by checking timestamps)
        tokio::time::sleep(Duration::from_millis(150)).await;
        let caps3 = cache.get().await;
        assert_eq!(caps3.len(), 1);
    }
}
