# Chronik Stream Roadmap

This document provides detailed implementation plans for all pending features in Chronik Stream.

**Current Version**: v2.2.21
**Last Updated**: 2025-11-30

---

## Completed Phases

| Phase | Description | Version | Status |
|-------|-------------|---------|--------|
| Phase 0 | Dead Code Cleanup (~10,405 lines) | - | ✅ Complete |
| Phase 1 | Data Integrity (CRC32, LZ4/Snappy) | v2.2.18 | ✅ Complete |
| Phase 2 | Security (Backup Encryption) | v2.2.19 | ✅ Complete |
| Phase 3 | Topic Deletion | v2.2.20 | ✅ Complete |
| Phase 3b | API Completeness (AlterConfigs, Consumer Groups) | v2.2.21 | ✅ Complete |

---

## Phase 4: Performance Optimizations

### 4.1 Segment Cache Layer

**Priority**: P4
**Effort**: 1-2 days
**Impact**: High for read-heavy workloads

#### Problem Statement

Currently, every fetch request reads directly from storage (disk or object store). For frequently accessed segments, this adds unnecessary latency and I/O overhead.

#### Proposed Solution

Implement an LRU cache that stores recently accessed segments in memory.

#### Data Structures

```rust
// Location: crates/chronik-storage/src/segment_cache.rs (new file)

use lru::LruCache;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use parking_lot::RwLock;
use bytes::Bytes;

/// Cache key for segment lookup
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CacheKey {
    pub topic: String,
    pub partition: i32,
    pub base_offset: i64,
}

/// Cached segment data with metadata
pub struct CachedSegment {
    pub data: Bytes,
    pub loaded_at: std::time::Instant,
    pub access_count: AtomicU64,
    pub size_bytes: usize,
}

/// Segment cache with LRU eviction
pub struct SegmentCache {
    /// LRU cache entries
    entries: RwLock<LruCache<CacheKey, CachedSegment>>,
    /// Maximum cache size in bytes
    max_size_bytes: usize,
    /// Current cache size
    current_size: AtomicUsize,
    /// Cache hit counter
    hit_count: AtomicU64,
    /// Cache miss counter
    miss_count: AtomicU64,
}

impl SegmentCache {
    pub fn new(max_size_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(LruCache::new(
                std::num::NonZeroUsize::new(max_entries).unwrap()
            )),
            max_size_bytes,
            current_size: AtomicUsize::new(0),
            hit_count: AtomicU64::new(0),
            miss_count: AtomicU64::new(0),
        }
    }

    /// Get segment from cache
    pub fn get(&self, key: &CacheKey) -> Option<Bytes> {
        let mut cache = self.entries.write();
        if let Some(entry) = cache.get(key) {
            entry.access_count.fetch_add(1, Ordering::Relaxed);
            self.hit_count.fetch_add(1, Ordering::Relaxed);
            Some(entry.data.clone())
        } else {
            self.miss_count.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Insert segment into cache
    pub fn insert(&self, key: CacheKey, data: Bytes) {
        let size = data.len();

        // Evict entries if needed to make room
        while self.current_size.load(Ordering::Relaxed) + size > self.max_size_bytes {
            let mut cache = self.entries.write();
            if let Some((_, evicted)) = cache.pop_lru() {
                self.current_size.fetch_sub(evicted.size_bytes, Ordering::Relaxed);
            } else {
                break;
            }
        }

        let entry = CachedSegment {
            data,
            loaded_at: std::time::Instant::now(),
            access_count: AtomicU64::new(1),
            size_bytes: size,
        };

        let mut cache = self.entries.write();
        cache.put(key, entry);
        self.current_size.fetch_add(size, Ordering::Relaxed);
    }

    /// Invalidate segment (e.g., after rotation)
    pub fn invalidate(&self, key: &CacheKey) {
        let mut cache = self.entries.write();
        if let Some(evicted) = cache.pop(key) {
            self.current_size.fetch_sub(evicted.size_bytes, Ordering::Relaxed);
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let hits = self.hit_count.load(Ordering::Relaxed);
        let misses = self.miss_count.load(Ordering::Relaxed);
        let total = hits + misses;

        CacheStats {
            hit_count: hits,
            miss_count: misses,
            hit_rate: if total > 0 { hits as f64 / total as f64 } else { 0.0 },
            current_size_bytes: self.current_size.load(Ordering::Relaxed),
            max_size_bytes: self.max_size_bytes,
            entry_count: self.entries.read().len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub hit_count: u64,
    pub miss_count: u64,
    pub hit_rate: f64,
    pub current_size_bytes: usize,
    pub max_size_bytes: usize,
    pub entry_count: usize,
}
```

#### Integration Points

1. **SegmentReader** (`crates/chronik-storage/src/segment_reader.rs`):
   - Check cache before storage read
   - Populate cache after storage read
   - Invalidate on segment rotation

2. **FetchHandler** (`crates/chronik-server/src/fetch_handler.rs`):
   - Pass cache reference to segment reader
   - Log cache hit/miss rates

3. **Configuration**:
   ```bash
   CHRONIK_CACHE_ENABLED=true
   CHRONIK_CACHE_SIZE_MB=512          # Default: 512MB
   CHRONIK_CACHE_MAX_ENTRIES=10000    # Default: 10K segments
   ```

#### Metrics to Expose

- `chronik_cache_hit_total` - Total cache hits
- `chronik_cache_miss_total` - Total cache misses
- `chronik_cache_hit_rate` - Current hit rate (0.0-1.0)
- `chronik_cache_size_bytes` - Current cache size
- `chronik_cache_evictions_total` - Total evictions

---

### 4.2 Vector Search HNSW

**Priority**: P4
**Effort**: 3-5 days (or 1 day with existing crate)
**Impact**: High for semantic search use cases

#### Problem Statement

Current vector search uses brute-force O(n) search, which doesn't scale beyond ~10K vectors.

#### Proposed Solution

Implement Hierarchical Navigable Small World (HNSW) graphs for O(log n) approximate nearest neighbor search.

#### Option A: Use Existing Crate (Recommended)

```toml
# Add to crates/chronik-storage/Cargo.toml
[dependencies]
instant-distance = "0.6"  # Battle-tested HNSW implementation
```

```rust
// Location: crates/chronik-storage/src/vector_search.rs

use instant_distance::{Builder, Search, HnswMap};
use serde::{Deserialize, Serialize};

/// Vector index using HNSW
pub struct VectorIndex {
    /// HNSW index
    hnsw: HnswMap<Point, u64>,  // Point -> record offset
    /// Index configuration
    config: VectorIndexConfig,
}

#[derive(Clone)]
pub struct Point(pub Vec<f32>);

impl instant_distance::Point for Point {
    fn distance(&self, other: &Self) -> f32 {
        // Euclidean distance
        self.0.iter()
            .zip(other.0.iter())
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            .sqrt()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexConfig {
    /// Number of dimensions
    pub dimensions: usize,
    /// Number of neighbors per node (M parameter)
    pub m: usize,
    /// Size of dynamic candidate list (ef_construction)
    pub ef_construction: usize,
    /// Search accuracy vs speed tradeoff
    pub ef_search: usize,
}

impl Default for VectorIndexConfig {
    fn default() -> Self {
        Self {
            dimensions: 768,        // Common embedding size
            m: 16,                  // Neighbors per node
            ef_construction: 200,   // Build-time accuracy
            ef_search: 50,          // Query-time accuracy
        }
    }
}

impl VectorIndex {
    pub fn new(config: VectorIndexConfig) -> Self {
        Self {
            hnsw: HnswMap::default(),
            config,
        }
    }

    /// Build index from vectors
    pub fn build(vectors: Vec<(Vec<f32>, u64)>, config: VectorIndexConfig) -> Self {
        let points: Vec<Point> = vectors.iter().map(|(v, _)| Point(v.clone())).collect();
        let values: Vec<u64> = vectors.iter().map(|(_, id)| *id).collect();

        let hnsw = Builder::default()
            .ef_construction(config.ef_construction)
            .build(points, values);

        Self { hnsw, config }
    }

    /// Search for k nearest neighbors
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        let query_point = Point(query.to_vec());
        let mut search = Search::default();

        self.hnsw
            .search(&query_point, &mut search)
            .take(k)
            .map(|item| (*item.value, item.distance))
            .collect()
    }

    /// Serialize index for persistence
    pub fn serialize(&self) -> Result<Vec<u8>, bincode::Error> {
        // Note: instant-distance supports serde
        bincode::serialize(&self.hnsw)
    }

    /// Deserialize index from bytes
    pub fn deserialize(data: &[u8], config: VectorIndexConfig) -> Result<Self, bincode::Error> {
        let hnsw = bincode::deserialize(data)?;
        Ok(Self { hnsw, config })
    }
}
```

#### Option B: Implement from Scratch

Only if custom requirements exist. See PENDING_IMPLEMENTATIONS.md for full algorithm details.

#### Integration Points

1. **Tantivy Indexer** - Build HNSW index alongside text index
2. **Search API** - Add vector similarity endpoint
3. **Segment Writer** - Persist HNSW index to object store

#### Performance Characteristics

| Dataset Size | Brute Force | HNSW |
|--------------|-------------|------|
| 1K vectors | 1ms | 0.1ms |
| 10K vectors | 10ms | 0.2ms |
| 100K vectors | 100ms | 0.5ms |
| 1M vectors | 1000ms | 1ms |

---

### 4.3 Benchmarks

**Priority**: P4
**Effort**: 1 day
**Impact**: Medium (tooling/validation)

#### Problem Statement

Missing round-trip and metadata benchmarks make it difficult to validate performance improvements.

#### Implementation

```rust
// Location: crates/chronik-benchmarks/src/benchmark.rs

use std::time::{Duration, Instant};
use kafka::producer::{Producer, Record};
use kafka::consumer::{Consumer, FetchOffset};

/// Round-trip benchmark: produce → consume latency
pub async fn benchmark_roundtrip(config: &BenchmarkConfig) -> BenchmarkResult {
    let producer = Producer::from_hosts(vec![config.bootstrap_server.clone()])
        .create()
        .expect("Producer creation failed");

    let mut consumer = Consumer::from_hosts(vec![config.bootstrap_server.clone()])
        .with_topic(config.topic.clone())
        .with_fallback_offset(FetchOffset::Latest)
        .create()
        .expect("Consumer creation failed");

    let mut latencies = Vec::with_capacity(config.message_count);

    for i in 0..config.message_count {
        let key = format!("key-{}", i);
        let value = config.payload.clone();

        let start = Instant::now();

        // Produce
        producer.send(&Record::from_key_value(&config.topic, key.as_bytes(), &value))
            .expect("Send failed");

        // Consume until we find our message
        loop {
            for ms in consumer.poll().unwrap().iter() {
                for m in ms.messages() {
                    if m.key == key.as_bytes() {
                        latencies.push(start.elapsed());
                        break;
                    }
                }
            }
            if latencies.len() > i {
                break;
            }
        }
    }

    BenchmarkResult::from_latencies("roundtrip", latencies)
}

/// Metadata benchmark: topic operations
pub async fn benchmark_metadata(config: &BenchmarkConfig) -> BenchmarkResult {
    let admin = AdminClient::new(config.bootstrap_server.clone());
    let mut latencies = Vec::new();

    // Create topics
    for i in 0..config.topic_count {
        let topic = format!("bench-topic-{}", i);
        let start = Instant::now();
        admin.create_topic(&topic, 3, 1).await.ok();
        latencies.push(("create", start.elapsed()));
    }

    // Describe topics
    for i in 0..config.topic_count {
        let topic = format!("bench-topic-{}", i);
        let start = Instant::now();
        admin.describe_topic(&topic).await.ok();
        latencies.push(("describe", start.elapsed()));
    }

    // List topics
    let start = Instant::now();
    admin.list_topics().await.ok();
    latencies.push(("list", start.elapsed()));

    // Delete topics
    for i in 0..config.topic_count {
        let topic = format!("bench-topic-{}", i);
        let start = Instant::now();
        admin.delete_topic(&topic).await.ok();
        latencies.push(("delete", start.elapsed()));
    }

    BenchmarkResult::from_operations(latencies)
}

#[derive(Debug)]
pub struct BenchmarkResult {
    pub name: String,
    pub count: usize,
    pub total_duration: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub min: Duration,
    pub max: Duration,
    pub throughput: f64,  // ops/sec
}

impl BenchmarkResult {
    pub fn from_latencies(name: &str, mut latencies: Vec<Duration>) -> Self {
        latencies.sort();
        let count = latencies.len();
        let total: Duration = latencies.iter().sum();

        Self {
            name: name.to_string(),
            count,
            total_duration: total,
            p50: latencies[count / 2],
            p95: latencies[count * 95 / 100],
            p99: latencies[count * 99 / 100],
            min: latencies[0],
            max: latencies[count - 1],
            throughput: count as f64 / total.as_secs_f64(),
        }
    }
}
```

#### CLI Interface

```bash
# Run all benchmarks
cargo run --bin chronik-bench -- --bootstrap localhost:9092

# Run specific benchmark
cargo run --bin chronik-bench -- roundtrip --messages 10000 --payload-size 1024

# Run metadata benchmark
cargo run --bin chronik-bench -- metadata --topics 100

# Output formats
cargo run --bin chronik-bench -- --format json > results.json
cargo run --bin chronik-bench -- --format csv > results.csv
```

---

## Phase 5: Enterprise Features

### 5.1 TLS for Kafka Protocol

**Priority**: P5
**Effort**: 2-3 days
**Impact**: Required for production deployments

#### Problem Statement

Kafka traffic is unencrypted, exposing message content and credentials on the network.

#### Implementation

```rust
// Location: crates/chronik-server/src/tls.rs (new file)

use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// TLS configuration
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Enable TLS
    pub enabled: bool,
    /// Path to server certificate (PEM format)
    pub cert_path: String,
    /// Path to private key (PEM format)
    pub key_path: String,
    /// Path to CA certificate for client verification (mTLS)
    pub ca_path: Option<String>,
    /// Require client certificates (mTLS)
    pub require_client_cert: bool,
}

impl TlsConfig {
    /// Load from environment variables
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var("CHRONIK_TLS_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if !enabled {
            return None;
        }

        Some(Self {
            enabled: true,
            cert_path: std::env::var("CHRONIK_TLS_CERT")
                .expect("CHRONIK_TLS_CERT required when TLS enabled"),
            key_path: std::env::var("CHRONIK_TLS_KEY")
                .expect("CHRONIK_TLS_KEY required when TLS enabled"),
            ca_path: std::env::var("CHRONIK_TLS_CA").ok(),
            require_client_cert: std::env::var("CHRONIK_TLS_REQUIRE_CLIENT_CERT")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }

    /// Build TLS acceptor
    pub fn build_acceptor(&self) -> Result<TlsAcceptor, TlsError> {
        // Load certificate chain
        let cert_file = File::open(&self.cert_path)
            .map_err(|e| TlsError::CertificateLoad(e.to_string()))?;
        let mut cert_reader = BufReader::new(cert_file);
        let certs: Vec<Certificate> = certs(&mut cert_reader)
            .map_err(|e| TlsError::CertificateLoad(e.to_string()))?
            .into_iter()
            .map(Certificate)
            .collect();

        // Load private key
        let key_file = File::open(&self.key_path)
            .map_err(|e| TlsError::KeyLoad(e.to_string()))?;
        let mut key_reader = BufReader::new(key_file);

        // Try PKCS8 first, then RSA
        let keys = pkcs8_private_keys(&mut key_reader)
            .map_err(|e| TlsError::KeyLoad(e.to_string()))?;

        let key = if keys.is_empty() {
            // Retry with RSA format
            let key_file = File::open(&self.key_path)?;
            let mut key_reader = BufReader::new(key_file);
            let rsa_keys = rsa_private_keys(&mut key_reader)
                .map_err(|e| TlsError::KeyLoad(e.to_string()))?;
            PrivateKey(rsa_keys.into_iter().next()
                .ok_or(TlsError::KeyLoad("No private key found".into()))?)
        } else {
            PrivateKey(keys.into_iter().next().unwrap())
        };

        // Build server config
        let mut config = ServerConfig::builder()
            .with_safe_defaults();

        // Configure client authentication
        let config = if let Some(ca_path) = &self.ca_path {
            let ca_file = File::open(ca_path)?;
            let mut ca_reader = BufReader::new(ca_file);
            let ca_certs: Vec<Certificate> = certs(&mut ca_reader)?
                .into_iter()
                .map(Certificate)
                .collect();

            let mut root_store = rustls::RootCertStore::empty();
            for cert in ca_certs {
                root_store.add(&cert)?;
            }

            if self.require_client_cert {
                config.with_client_cert_verifier(
                    rustls::server::AllowAnyAuthenticatedClient::new(root_store).boxed()
                )
            } else {
                config.with_client_cert_verifier(
                    rustls::server::AllowAnyAnonymousOrAuthenticatedClient::new(root_store).boxed()
                )
            }
        } else {
            config.with_no_client_auth()
        };

        let config = config.with_single_cert(certs, key)
            .map_err(|e| TlsError::Configuration(e.to_string()))?;

        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("Failed to load certificate: {0}")]
    CertificateLoad(String),
    #[error("Failed to load private key: {0}")]
    KeyLoad(String),
    #[error("TLS configuration error: {0}")]
    Configuration(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
```

#### Integration into Server

```rust
// In crates/chronik-server/src/integrated_server/builder.rs

impl IntegratedKafkaServerBuilder {
    pub async fn build_listener(&self) -> Result<Listener> {
        let listener = TcpListener::bind(&self.config.bind_addr).await?;

        if let Some(tls_config) = TlsConfig::from_env() {
            let acceptor = tls_config.build_acceptor()?;
            Ok(Listener::Tls { listener, acceptor })
        } else {
            Ok(Listener::Plain { listener })
        }
    }
}

pub enum Listener {
    Plain { listener: TcpListener },
    Tls { listener: TcpListener, acceptor: TlsAcceptor },
}

impl Listener {
    pub async fn accept(&self) -> Result<Connection> {
        match self {
            Listener::Plain { listener } => {
                let (stream, addr) = listener.accept().await?;
                Ok(Connection::Plain(stream, addr))
            }
            Listener::Tls { listener, acceptor } => {
                let (stream, addr) = listener.accept().await?;
                let tls_stream = acceptor.accept(stream).await?;
                Ok(Connection::Tls(tls_stream, addr))
            }
        }
    }
}
```

#### Environment Variables

| Variable | Description | Required |
|----------|-------------|----------|
| `CHRONIK_TLS_ENABLED` | Enable TLS (`true`/`false`) | No (default: false) |
| `CHRONIK_TLS_CERT` | Path to server certificate | If TLS enabled |
| `CHRONIK_TLS_KEY` | Path to private key | If TLS enabled |
| `CHRONIK_TLS_CA` | Path to CA cert for client verification | No (for mTLS) |
| `CHRONIK_TLS_REQUIRE_CLIENT_CERT` | Require client certs | No (default: false) |

#### Testing

```bash
# Generate self-signed certificates for testing
openssl req -x509 -newkey rsa:4096 -keyout server.key -out server.crt -days 365 -nodes

# Start server with TLS
CHRONIK_TLS_ENABLED=true \
CHRONIK_TLS_CERT=server.crt \
CHRONIK_TLS_KEY=server.key \
./chronik-server start

# Test with kafka-python
from kafka import KafkaProducer
import ssl

context = ssl.create_default_context()
context.load_verify_locations('server.crt')

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    security_protocol='SSL',
    ssl_context=context
)
```

---

### 5.2 ACL Authorization System

**Priority**: P5
**Effort**: 3-5 days
**Impact**: Required for multi-tenant deployments

#### Problem Statement

Currently any authenticated user can perform any operation. Need fine-grained access control.

#### Data Model

```rust
// Location: crates/chronik-server/src/acl/mod.rs (new file)

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Resource types that can be protected
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ResourceType {
    Topic,
    Group,
    Cluster,
    TransactionalId,
    DelegationToken,
}

/// Pattern type for resource matching
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PatternType {
    /// Exact match
    Literal,
    /// Prefix match (e.g., "logs-" matches "logs-app1", "logs-app2")
    Prefixed,
}

/// Operations that can be performed
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AclOperation {
    Unknown,
    Any,
    All,
    Read,
    Write,
    Create,
    Delete,
    Alter,
    Describe,
    ClusterAction,
    DescribeConfigs,
    AlterConfigs,
    IdempotentWrite,
}

/// Permission type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AclPermission {
    Allow,
    Deny,
}

/// ACL binding - ties a principal to permissions on a resource
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AclBinding {
    /// Resource type (Topic, Group, etc.)
    pub resource_type: ResourceType,
    /// Resource name ("my-topic", "*" for all)
    pub resource_name: String,
    /// Pattern type (Literal, Prefixed)
    pub pattern_type: PatternType,
    /// Principal ("User:alice", "User:*" for all)
    pub principal: String,
    /// Host ("*" for any, specific IP for restriction)
    pub host: String,
    /// Operation (Read, Write, etc.)
    pub operation: AclOperation,
    /// Permission (Allow, Deny)
    pub permission: AclPermission,
}

/// ACL filter for querying/deleting
#[derive(Debug, Clone, Default)]
pub struct AclFilter {
    pub resource_type: Option<ResourceType>,
    pub resource_name: Option<String>,
    pub pattern_type: Option<PatternType>,
    pub principal: Option<String>,
    pub host: Option<String>,
    pub operation: Option<AclOperation>,
    pub permission: Option<AclPermission>,
}

impl AclBinding {
    /// Check if this binding matches the filter
    pub fn matches(&self, filter: &AclFilter) -> bool {
        filter.resource_type.map_or(true, |rt| rt == self.resource_type)
            && filter.resource_name.as_ref().map_or(true, |rn| rn == &self.resource_name)
            && filter.pattern_type.map_or(true, |pt| pt == self.pattern_type)
            && filter.principal.as_ref().map_or(true, |p| p == &self.principal)
            && filter.host.as_ref().map_or(true, |h| h == &self.host)
            && filter.operation.map_or(true, |op| op == self.operation)
            && filter.permission.map_or(true, |perm| perm == self.permission)
    }
}
```

#### ACL Manager

```rust
// Location: crates/chronik-server/src/acl/manager.rs

use super::*;
use crate::metadata::MetadataStore;
use parking_lot::RwLock;
use std::sync::Arc;

/// ACL Manager handles authorization checks
pub struct AclManager {
    /// All ACL bindings
    acls: RwLock<Vec<AclBinding>>,
    /// Metadata store for persistence
    metadata_store: Arc<dyn MetadataStore>,
    /// Superusers who bypass all ACL checks
    superusers: HashSet<String>,
    /// Whether ACLs are enabled
    enabled: bool,
}

impl AclManager {
    pub fn new(
        metadata_store: Arc<dyn MetadataStore>,
        superusers: Vec<String>,
        enabled: bool,
    ) -> Self {
        Self {
            acls: RwLock::new(Vec::new()),
            metadata_store,
            superusers: superusers.into_iter().collect(),
            enabled,
        }
    }

    /// Load ACLs from metadata store on startup
    pub async fn load(&self) -> Result<(), AclError> {
        let acls = self.metadata_store.list_acls().await?;
        *self.acls.write() = acls;
        tracing::info!("Loaded {} ACL bindings", self.acls.read().len());
        Ok(())
    }

    /// Check if principal has permission for operation on resource
    pub fn check_permission(
        &self,
        principal: &str,
        host: &str,
        resource_type: ResourceType,
        resource_name: &str,
        operation: AclOperation,
    ) -> bool {
        // If ACLs disabled, allow everything
        if !self.enabled {
            return true;
        }

        // Superusers bypass all checks
        if self.superusers.contains(principal) {
            return true;
        }

        let acls = self.acls.read();

        // Check for explicit DENY first (deny takes precedence)
        for acl in acls.iter() {
            if acl.permission == AclPermission::Deny
                && self.matches_resource(acl, resource_type, resource_name)
                && self.matches_principal(acl, principal)
                && self.matches_host(acl, host)
                && self.matches_operation(acl, operation)
            {
                return false;
            }
        }

        // Check for ALLOW
        for acl in acls.iter() {
            if acl.permission == AclPermission::Allow
                && self.matches_resource(acl, resource_type, resource_name)
                && self.matches_principal(acl, principal)
                && self.matches_host(acl, host)
                && self.matches_operation(acl, operation)
            {
                return true;
            }
        }

        // Default deny
        false
    }

    fn matches_resource(&self, acl: &AclBinding, resource_type: ResourceType, resource_name: &str) -> bool {
        if acl.resource_type != resource_type {
            return false;
        }

        match acl.pattern_type {
            PatternType::Literal => {
                acl.resource_name == "*" || acl.resource_name == resource_name
            }
            PatternType::Prefixed => {
                resource_name.starts_with(&acl.resource_name)
            }
        }
    }

    fn matches_principal(&self, acl: &AclBinding, principal: &str) -> bool {
        acl.principal == "User:*" || acl.principal == principal
    }

    fn matches_host(&self, acl: &AclBinding, host: &str) -> bool {
        acl.host == "*" || acl.host == host
    }

    fn matches_operation(&self, acl: &AclBinding, operation: AclOperation) -> bool {
        acl.operation == AclOperation::All || acl.operation == operation
    }

    /// Create ACL
    pub async fn create_acl(&self, binding: AclBinding) -> Result<(), AclError> {
        self.metadata_store.create_acl(binding.clone()).await?;
        self.acls.write().push(binding);
        Ok(())
    }

    /// Delete ACLs matching filter
    pub async fn delete_acls(&self, filter: &AclFilter) -> Result<Vec<AclBinding>, AclError> {
        let deleted = self.metadata_store.delete_acls(filter).await?;
        self.acls.write().retain(|acl| !acl.matches(filter));
        Ok(deleted)
    }

    /// List ACLs matching filter
    pub fn list_acls(&self, filter: &AclFilter) -> Vec<AclBinding> {
        self.acls.read()
            .iter()
            .filter(|acl| acl.matches(filter))
            .cloned()
            .collect()
    }
}
```

#### MetadataStore Extensions

```rust
// Add to crates/chronik-common/src/metadata/traits.rs

#[async_trait]
pub trait MetadataStore: Send + Sync {
    // ... existing methods ...

    // ACL operations
    async fn create_acl(&self, binding: AclBinding) -> Result<()>;
    async fn delete_acls(&self, filter: &AclFilter) -> Result<Vec<AclBinding>>;
    async fn list_acls(&self) -> Result<Vec<AclBinding>>;
}
```

#### Kafka API Implementation

```rust
// In crates/chronik-protocol/src/handler.rs

/// Handle CreateAcls (API Key 30)
async fn handle_create_acls(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
    let request = CreateAclsRequest::decode(body, header.api_version)?;

    let mut results = Vec::new();
    for creation in request.creations {
        let binding = AclBinding {
            resource_type: creation.resource_type.into(),
            resource_name: creation.resource_name,
            pattern_type: creation.pattern_type.into(),
            principal: creation.principal,
            host: creation.host,
            operation: creation.operation.into(),
            permission: creation.permission.into(),
        };

        let result = match self.acl_manager.create_acl(binding).await {
            Ok(()) => AclCreationResult { error_code: 0, error_message: None },
            Err(e) => AclCreationResult { error_code: -1, error_message: Some(e.to_string()) },
        };
        results.push(result);
    }

    let response = CreateAclsResponse { results };
    Ok(Response::CreateAcls(response))
}

/// Handle DeleteAcls (API Key 31)
async fn handle_delete_acls(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
    // Similar implementation...
}

/// Handle DescribeAcls (API Key 29)
async fn handle_describe_acls(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
    // Similar implementation...
}
```

#### Configuration

```bash
# Enable ACL enforcement
CHRONIK_ACL_ENABLED=true

# Superusers (comma-separated, bypass all ACL checks)
CHRONIK_SUPERUSERS=admin,kafka-admin

# Default: deny all if no matching ACL (more secure)
# Alternative: allow all if no matching ACL (less secure, for migration)
CHRONIK_ACL_DEFAULT_DENY=true
```

---

### 5.3 Schema Registry

**Priority**: P5
**Effort**: 5-7 days
**Impact**: High for data-heavy use cases with Avro/Protobuf

#### Problem Statement

Clients using Avro, Protobuf, or JSON Schema serialization need a schema registry for:
- Schema storage and versioning
- Compatibility checking
- Schema evolution

#### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Schema Registry                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐       │
│  │  REST API   │   │   Storage   │   │ Compatibility│      │
│  │  (HTTP)     │   │  (WAL-based)│   │   Checker    │      │
│  └─────────────┘   └─────────────┘   └─────────────┘       │
│         │                 │                  │              │
│         └─────────────────┼──────────────────┘              │
│                           │                                 │
│                    ┌──────┴──────┐                         │
│                    │  Schema     │                         │
│                    │  Manager    │                         │
│                    └─────────────┘                         │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

#### Data Model

```rust
// Location: crates/chronik-server/src/schema_registry/mod.rs (new module)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Schema type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SchemaType {
    Avro,
    Protobuf,
    Json,
}

/// Schema reference (for schema dependencies)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaReference {
    /// Reference name
    pub name: String,
    /// Subject containing the referenced schema
    pub subject: String,
    /// Version of the referenced schema
    pub version: i32,
}

/// Registered schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    /// Global unique ID
    pub id: i32,
    /// Version within subject
    pub version: i32,
    /// Subject name (e.g., "orders-value")
    pub subject: String,
    /// Schema type
    pub schema_type: SchemaType,
    /// Schema definition (JSON string)
    pub schema: String,
    /// Schema references (for complex types)
    pub references: Vec<SchemaReference>,
    /// Fingerprint for deduplication
    pub fingerprint: String,
}

/// Compatibility level
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum CompatibilityLevel {
    /// No compatibility checking
    None,
    /// New schema can read data written by old schema
    #[default]
    Backward,
    /// Old schema can read data written by new schema
    Forward,
    /// Both backward and forward compatible
    Full,
    /// Backward compatible with all previous versions
    BackwardTransitive,
    /// Forward compatible with all previous versions
    ForwardTransitive,
    /// Full compatible with all previous versions
    FullTransitive,
}

/// Subject configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectConfig {
    pub compatibility: CompatibilityLevel,
}
```

#### REST API

```rust
// Location: crates/chronik-server/src/schema_registry/api.rs

use axum::{
    extract::{Path, State, Json},
    routing::{get, post, put, delete},
    Router,
};
use std::sync::Arc;

pub fn schema_registry_router(manager: Arc<SchemaManager>) -> Router {
    Router::new()
        // Subjects
        .route("/subjects", get(list_subjects))
        .route("/subjects/:subject", delete(delete_subject))
        .route("/subjects/:subject/versions", get(list_versions))
        .route("/subjects/:subject/versions", post(register_schema))
        .route("/subjects/:subject/versions/:version", get(get_schema_by_version))
        .route("/subjects/:subject/versions/:version", delete(delete_version))

        // Schemas by ID
        .route("/schemas/ids/:id", get(get_schema_by_id))
        .route("/schemas/ids/:id/schema", get(get_raw_schema))
        .route("/schemas/ids/:id/subjects", get(get_subjects_by_schema))

        // Compatibility
        .route("/compatibility/subjects/:subject/versions/:version",
               post(check_compatibility))
        .route("/compatibility/subjects/:subject/versions",
               post(check_compatibility_latest))

        // Config
        .route("/config", get(get_global_config))
        .route("/config", put(set_global_config))
        .route("/config/:subject", get(get_subject_config))
        .route("/config/:subject", put(set_subject_config))
        .route("/config/:subject", delete(delete_subject_config))

        // Mode (for migrations)
        .route("/mode", get(get_mode))
        .route("/mode", put(set_mode))

        .with_state(manager)
}

/// Register a new schema
async fn register_schema(
    State(manager): State<Arc<SchemaManager>>,
    Path(subject): Path<String>,
    Json(request): Json<RegisterSchemaRequest>,
) -> Result<Json<RegisterSchemaResponse>, SchemaRegistryError> {
    let schema_id = manager.register_schema(
        &subject,
        &request.schema,
        request.schema_type.unwrap_or(SchemaType::Avro),
        request.references.unwrap_or_default(),
    ).await?;

    Ok(Json(RegisterSchemaResponse { id: schema_id }))
}

/// Get schema by global ID
async fn get_schema_by_id(
    State(manager): State<Arc<SchemaManager>>,
    Path(id): Path<i32>,
) -> Result<Json<SchemaResponse>, SchemaRegistryError> {
    let schema = manager.get_schema_by_id(id).await?
        .ok_or(SchemaRegistryError::SchemaNotFound(id))?;

    Ok(Json(SchemaResponse {
        schema: schema.schema,
        schema_type: Some(schema.schema_type),
        references: Some(schema.references),
    }))
}

/// Check compatibility
async fn check_compatibility(
    State(manager): State<Arc<SchemaManager>>,
    Path((subject, version)): Path<(String, String)>,
    Json(request): Json<CompatibilityCheckRequest>,
) -> Result<Json<CompatibilityCheckResponse>, SchemaRegistryError> {
    let version = if version == "latest" {
        None
    } else {
        Some(version.parse::<i32>()?)
    };

    let is_compatible = manager.check_compatibility(
        &subject,
        &request.schema,
        version,
    ).await?;

    Ok(Json(CompatibilityCheckResponse { is_compatible }))
}

#[derive(Deserialize)]
struct RegisterSchemaRequest {
    schema: String,
    #[serde(rename = "schemaType")]
    schema_type: Option<SchemaType>,
    references: Option<Vec<SchemaReference>>,
}

#[derive(Serialize)]
struct RegisterSchemaResponse {
    id: i32,
}
```

#### Compatibility Checker

```rust
// Location: crates/chronik-server/src/schema_registry/compatibility.rs

use apache_avro::Schema as AvroSchema;

pub struct CompatibilityChecker;

impl CompatibilityChecker {
    /// Check if new schema is compatible with existing schemas
    pub fn check(
        new_schema: &str,
        existing_schemas: &[String],
        schema_type: SchemaType,
        compatibility: CompatibilityLevel,
    ) -> Result<bool, CompatibilityError> {
        match schema_type {
            SchemaType::Avro => Self::check_avro(new_schema, existing_schemas, compatibility),
            SchemaType::Protobuf => Self::check_protobuf(new_schema, existing_schemas, compatibility),
            SchemaType::Json => Self::check_json(new_schema, existing_schemas, compatibility),
        }
    }

    fn check_avro(
        new_schema: &str,
        existing_schemas: &[String],
        compatibility: CompatibilityLevel,
    ) -> Result<bool, CompatibilityError> {
        let new = AvroSchema::parse_str(new_schema)
            .map_err(|e| CompatibilityError::InvalidSchema(e.to_string()))?;

        let schemas_to_check = match compatibility {
            CompatibilityLevel::None => return Ok(true),
            CompatibilityLevel::Backward | CompatibilityLevel::Forward | CompatibilityLevel::Full => {
                // Check against latest only
                existing_schemas.last().map(|s| vec![s.as_str()]).unwrap_or_default()
            }
            CompatibilityLevel::BackwardTransitive |
            CompatibilityLevel::ForwardTransitive |
            CompatibilityLevel::FullTransitive => {
                // Check against all versions
                existing_schemas.iter().map(|s| s.as_str()).collect()
            }
        };

        for existing_str in schemas_to_check {
            let existing = AvroSchema::parse_str(existing_str)
                .map_err(|e| CompatibilityError::InvalidSchema(e.to_string()))?;

            let compatible = match compatibility {
                CompatibilityLevel::Backward | CompatibilityLevel::BackwardTransitive => {
                    // New schema can read old data
                    Self::can_read(&new, &existing)
                }
                CompatibilityLevel::Forward | CompatibilityLevel::ForwardTransitive => {
                    // Old schema can read new data
                    Self::can_read(&existing, &new)
                }
                CompatibilityLevel::Full | CompatibilityLevel::FullTransitive => {
                    // Both directions
                    Self::can_read(&new, &existing) && Self::can_read(&existing, &new)
                }
                CompatibilityLevel::None => true,
            };

            if !compatible {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn can_read(reader: &AvroSchema, writer: &AvroSchema) -> bool {
        // Use avro's built-in compatibility check
        apache_avro::schema::ResolvedSchema::try_from(reader)
            .map(|resolved| resolved.resolve(writer).is_ok())
            .unwrap_or(false)
    }

    fn check_protobuf(
        _new_schema: &str,
        _existing_schemas: &[String],
        _compatibility: CompatibilityLevel,
    ) -> Result<bool, CompatibilityError> {
        // Protobuf compatibility rules:
        // - Adding optional fields is backward compatible
        // - Removing optional fields is forward compatible
        // - Changing field numbers is never compatible
        // - Changing wire types is never compatible
        todo!("Implement protobuf compatibility checking")
    }

    fn check_json(
        _new_schema: &str,
        _existing_schemas: &[String],
        _compatibility: CompatibilityLevel,
    ) -> Result<bool, CompatibilityError> {
        // JSON Schema compatibility is complex
        // Consider using jsonschema crate
        todo!("Implement JSON schema compatibility checking")
    }
}
```

#### Dependencies

```toml
# Add to crates/chronik-server/Cargo.toml
[dependencies]
apache-avro = "0.16"      # Avro schema parsing
axum = "0.7"               # HTTP framework (already have)
jsonschema = "0.18"        # JSON Schema validation
prost = "0.12"             # Protobuf (already have)
sha2 = "0.10"              # For schema fingerprinting
```

#### Configuration

```bash
# Enable schema registry
CHRONIK_SCHEMA_REGISTRY_ENABLED=true

# Port (default: 8081, standard schema registry port)
CHRONIK_SCHEMA_REGISTRY_PORT=8081

# Default compatibility level
CHRONIK_SCHEMA_COMPATIBILITY=BACKWARD

# Bind address
CHRONIK_SCHEMA_REGISTRY_BIND=0.0.0.0
```

---

### 5.4 GSSAPI/Kerberos Authentication

**Priority**: P5 (Low - only if enterprise Kerberos integration required)
**Effort**: 5-7 days
**Impact**: Enterprise environments with existing Kerberos

#### Problem Statement

Enterprise environments often use Kerberos for centralized authentication. GSSAPI (Generic Security Services API) provides the standard interface.

**Note**: This is complex and requires:
- Existing Kerberos infrastructure (KDC)
- Service principal for Chronik
- Keytab file

#### Implementation Sketch

```rust
// Location: crates/chronik-protocol/src/sasl/gssapi.rs (new file)

use libgssapi::{
    context::{ServerCtx, SecurityContext},
    credential::Cred,
    name::Name,
    oid::{OID, GSS_MECH_KRB5},
};

pub struct GssapiAuthenticator {
    /// Service principal (e.g., "kafka/hostname@REALM")
    service_principal: String,
    /// Path to keytab file
    keytab_path: String,
    /// Server credential
    credential: Option<Cred>,
}

impl GssapiAuthenticator {
    pub fn new(service_principal: String, keytab_path: String) -> Result<Self, GssapiError> {
        // Set KRB5_KTNAME environment variable
        std::env::set_var("KRB5_KTNAME", &keytab_path);

        // Acquire server credential
        let name = Name::new(
            service_principal.as_bytes(),
            Some(&GSS_NT_HOSTBASED_SERVICE),
        )?;

        let credential = Cred::acquire(
            Some(&name),
            None,  // Default lifetime
            vec![GSS_MECH_KRB5.clone()],
            libgssapi::credential::CredUsage::Accept,
        )?;

        Ok(Self {
            service_principal,
            keytab_path,
            credential: Some(credential),
        })
    }

    /// Process client token, return response token and optionally the authenticated principal
    pub fn step(
        &self,
        context: &mut Option<ServerCtx>,
        client_token: &[u8],
    ) -> Result<(Vec<u8>, Option<String>), GssapiError> {
        let cred = self.credential.as_ref()
            .ok_or(GssapiError::NotInitialized)?;

        let (ctx, token) = if let Some(ctx) = context.take() {
            // Continue existing context
            ctx.step(client_token)?
        } else {
            // Start new context
            ServerCtx::new(cred.clone())
                .step(client_token)?
        };

        let principal = if ctx.is_established() {
            // Extract authenticated principal name
            let src_name = ctx.source_name()?;
            Some(src_name.to_string())
        } else {
            None
        };

        *context = Some(ctx);
        Ok((token.unwrap_or_default(), principal))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GssapiError {
    #[error("GSSAPI error: {0}")]
    Gssapi(String),
    #[error("Not initialized")]
    NotInitialized,
    #[error("Authentication failed")]
    AuthenticationFailed,
}
```

#### Configuration

```bash
# Enable GSSAPI
CHRONIK_SASL_MECHANISMS=GSSAPI,PLAIN,SCRAM-SHA-256

# Service principal
CHRONIK_GSSAPI_SERVICE_PRINCIPAL=kafka/chronik-server.example.com@EXAMPLE.COM

# Keytab path
CHRONIK_GSSAPI_KEYTAB=/etc/security/keytabs/chronik.keytab
```

#### Dependencies

```toml
# Add to crates/chronik-protocol/Cargo.toml
[dependencies]
libgssapi = "0.7"  # Rust GSSAPI bindings

[target.'cfg(unix)'.dependencies]
# libgssapi requires system GSSAPI library
# On Debian/Ubuntu: apt install libkrb5-dev
# On RHEL/CentOS: yum install krb5-devel
```

---

## Implementation Priority Matrix

| Feature | Effort | Value | Dependencies | Recommendation |
|---------|--------|-------|--------------|----------------|
| Segment Cache | 1-2 days | High | None | Do first if fetch latency matters |
| TLS | 2-3 days | High | None | Required for production |
| ACLs | 3-5 days | High | SASL working | Required for multi-tenant |
| Benchmarks | 1 day | Medium | None | Good for validation |
| HNSW | 3-5 days | Medium | Vector search used | Only if semantic search needed |
| Schema Registry | 5-7 days | High | None | If using Avro/Protobuf |
| GSSAPI | 5-7 days | Low | Kerberos infra | Only for enterprise |

---

## Getting Started

To work on any of these features:

1. **Read this document** thoroughly
2. **Check existing code** for integration points
3. **Create a branch**: `git checkout -b feature/<feature-name>`
4. **Implement incrementally** with tests
5. **Update PENDING_IMPLEMENTATIONS.md** when complete
6. **Test with real clients** before marking complete

For questions or clarifications, check [CLAUDE.md](../CLAUDE.md) for project conventions.
