//! Per-namespace [`Memory`] cache for multi-namespace server-side use.
//!
//! `Memory` is bound to a single namespace at construction time. The Chronik
//! Unified API server needs to serve many `(tenant, agent, ...)` namespaces
//! over a single HTTP surface; `MemoryRegistry` lazily creates and caches a
//! `Memory` per namespace.
//!
//! Each `Memory` builds its own rdkafka producer + admin client. For Phase 1
//! we accept N producers for N namespaces; sharing a single producer across
//! namespaces is a Phase 2 multi-tenancy optimization (see AM-2.5).
//!
//! The cache never evicts. Namespace lifetimes are governed by tenant
//! lifecycle (handled at admin layer), not by LRU.
//!
//! TODO (small follow-up to AM-1.7): when init_namespace_full creates the
//! `mem.{type}.{tenant}` topics, set `searchable=true` and `columnar=true`
//! per-topic config so BM25 + SQL recall work out-of-the-box without
//! requiring `CHRONIK_DEFAULT_SEARCHABLE=true` on the server. Today the
//! topic config relies on the server-wide default; an agent-memory deploy
//! that omits the env var loses the BM25 channel silently. Found during
//! the AM-1.7 smoke test on 2026-06-28.

use crate::client::Memory;
use crate::error::Result;
use crate::extractor::Extractor;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

/// Bootstrap configuration shared by every cached [`Memory`] instance.
#[derive(Clone)]
pub struct RegistryConfig {
    /// Kafka bootstrap servers (e.g. `localhost:9092,host2:9092`).
    pub kafka_brokers: String,
    /// Chronik Unified API base URL (e.g. `http://localhost:6092`).
    pub chronik_api: String,
    /// Optional extractor shared across all cached `Memory` instances.
    pub extractor: Option<Arc<dyn Extractor>>,
    /// Override `Memory::builder().idempotency_capacity(...)`.
    pub idempotency_capacity: Option<usize>,
    /// Override `Memory::builder().idempotency_ttl(...)`.
    pub idempotency_ttl: Option<Duration>,
    /// Override `Memory::builder().request_timeout(...)`.
    pub request_timeout: Option<Duration>,
}

impl RegistryConfig {
    /// Construct a `RegistryConfig` with only the required fields set.
    pub fn new(kafka_brokers: impl Into<String>, chronik_api: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            chronik_api: chronik_api.into(),
            extractor: None,
            idempotency_capacity: None,
            idempotency_ttl: None,
            request_timeout: None,
        }
    }

    /// Attach an extractor that every cached `Memory` will share.
    pub fn with_extractor(mut self, e: Arc<dyn Extractor>) -> Self {
        self.extractor = Some(e);
        self
    }
}

impl std::fmt::Debug for RegistryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryConfig")
            .field("kafka_brokers", &self.kafka_brokers)
            .field("chronik_api", &self.chronik_api)
            .field("extractor", &self.extractor.as_ref().map(|e| e.id()))
            .field("idempotency_capacity", &self.idempotency_capacity)
            .field("idempotency_ttl", &self.idempotency_ttl)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

/// Per-namespace cache of `Memory` instances.
///
/// Construct once at server startup; `get_or_create` from each request handler.
#[derive(Debug)]
pub struct MemoryRegistry {
    config: RegistryConfig,
    instances: DashMap<String, Arc<Memory>>,
}

impl MemoryRegistry {
    /// Construct an empty registry. No `Memory` instances are built until
    /// the first `get_or_create` call.
    pub fn new(config: RegistryConfig) -> Self {
        info!(
            kafka_brokers = %config.kafka_brokers,
            chronik_api = %config.chronik_api,
            has_extractor = config.extractor.is_some(),
            "MemoryRegistry initialized"
        );
        Self {
            config,
            instances: DashMap::new(),
        }
    }

    /// Number of cached `Memory` instances.
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    /// `true` if no namespaces have been built yet.
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Return the cached `Memory` for `namespace`, building one if absent.
    ///
    /// Constructing a `Memory` opens a Kafka producer + admin client and may
    /// block briefly on broker handshake. Concurrent callers for the same
    /// namespace may build twice (last-writer-wins on insertion) — the cost
    /// is one extra producer drop, accepted to keep this lock-free.
    pub async fn get_or_create(&self, namespace: &str) -> Result<Arc<Memory>> {
        if let Some(existing) = self.instances.get(namespace) {
            return Ok(existing.clone());
        }
        debug!(namespace = %namespace, "building new Memory instance");

        let mut builder = Memory::builder()
            .chronik_kafka(self.config.kafka_brokers.clone())
            .chronik_api(self.config.chronik_api.clone())
            .namespace(namespace);
        if let Some(cap) = self.config.idempotency_capacity {
            builder = builder.idempotency_capacity(cap);
        }
        if let Some(ttl) = self.config.idempotency_ttl {
            builder = builder.idempotency_ttl(ttl);
        }
        if let Some(rt) = self.config.request_timeout {
            builder = builder.request_timeout(rt);
        }
        if let Some(ext) = &self.config.extractor {
            builder = builder.extractor_arc(ext.clone());
        }
        let mem = builder.build().await?;
        let arc = Arc::new(mem);
        self.instances
            .insert(namespace.to_string(), arc.clone());
        Ok(arc)
    }

    /// Drop a namespace's cached `Memory` (its producer is dropped on the
    /// last `Arc` going out of scope). Returns whether an entry was present.
    ///
    /// Intended for tenant deprovisioning. Not currently invoked by the HTTP
    /// surface; reserved for the admin layer (AM-2.5).
    pub fn forget_namespace(&self, namespace: &str) -> bool {
        self.instances.remove(namespace).is_some()
    }
}

impl MemoryRegistry {
    /// Test-only constructor that pre-seeds the cache.
    #[doc(hidden)]
    pub fn with_seed(config: RegistryConfig, seed: Vec<(String, Arc<Memory>)>) -> Self {
        let instances = DashMap::new();
        for (k, v) in seed {
            instances.insert(k, v);
        }
        Self { config, instances }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_config() -> RegistryConfig {
        RegistryConfig::new("localhost:9092", "http://localhost:6092")
    }

    #[test]
    fn registry_starts_empty() {
        let r = MemoryRegistry::new(dummy_config());
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
    }

    #[test]
    fn forget_namespace_returns_false_when_absent() {
        let r = MemoryRegistry::new(dummy_config());
        assert!(!r.forget_namespace("tenant:agent:bot:user:luis"));
    }

    // `get_or_create` requires a running Kafka broker; covered by
    // integration tests in tests/cluster_smoke.rs (gated by CHRONIK_INTEGRATION=1).
}
