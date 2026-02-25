# Scale Validation & Performance Hardening Roadmap

**Status**: Planned
**Target**: v2.5.0
**Prerequisites**: Phase 9 (Query Orchestrator) complete, OpenAI embeddings validated at 30K/132K scale
**Related**: [MARKET_COMPARISON.md](MARKET_COMPARISON.md), [SEARCH_RELEVANCE_PERFORMANCE.md](SEARCH_RELEVANCE_PERFORMANCE.md), [ROADMAP_RELEVANCE_ENGINE.md](ROADMAP_RELEVANCE_ENGINE.md)

---

## Motivation

Phase 9 delivered the Query Orchestrator with multi-topic, multi-backend fan-out and validated performance on a 3-node bare metal k8s cluster. The results are competitive with dedicated systems (see [MARKET_COMPARISON.md](MARKET_COMPARISON.md)), but three gaps remain:

1. **Vector search is embedding-bound** — Every query calls OpenAI (~372ms p50). Dedicated vector DBs serve cached/pre-indexed vectors in 5-50ms. We need query embedding caching.
2. **Scale is unvalidated beyond 150K docs** — Elasticsearch and Pinecone publish benchmarks at 1M-50M+. We need to prove Tantivy and HNSW hold up at real scale.
3. **Query serving is single-pod** — The k8s tests used a single standalone pod. Chronik supports multi-node clusters, but distributed query fan-out across nodes isn't implemented.

This roadmap closes all three gaps.

---

## SV-1 — Query Embedding Cache

**Goal**: Reduce vector/hybrid query latency from ~372ms to ~1-5ms for repeated queries.

**Impact**: Massive. In production, search queries follow a power law — a small number of queries account for most traffic. Cache hit rates of 30-60% are typical, meaning 30-60% of vector queries drop from 372ms to 1-5ms.

### Current State

Every vector search query follows this path ([vector_index.rs:1340-1353](crates/chronik-columnar/src/vector_index.rs#L1340)):

```
query_text → OpenAI API (200-500ms) → Vec<f32> → HNSW search (1-5ms) → results
```

No caching exists. The same query text embeds fresh every time.

### Design

Add an LRU cache with TTL at the `VectorSearchService` level:

```rust
pub struct QueryEmbeddingCache {
    cache: RwLock<HashMap<String, CachedEmbedding>>,
    max_entries: usize,     // Default: 10,000
    ttl: Duration,          // Default: 1 hour
    metrics: CacheMetrics,  // hit/miss/eviction counters
}

struct CachedEmbedding {
    vector: Vec<f32>,
    created_at: Instant,
    hit_count: u64,
}
```

**Cache key**: Normalized query text (lowercased, trimmed).

**Cache placement**: Inside `VectorSearchService::search_by_text()` at line 1348, before the `self.provider.embed()` call:

```rust
pub async fn search_by_text(&self, topic: &str, query_text: &str, k: usize, filters: Option<VectorSearchFilters>) -> Result<Vec<VectorSearchResult>> {
    // CHECK CACHE FIRST
    let cache_key = query_text.trim().to_lowercase();
    if let Some(cached) = self.embedding_cache.get(&cache_key).await {
        self.metrics.cache_hits.inc();
        return self.search_by_vector(topic, &cached.vector, k, filters).await;
    }

    // Cache miss — call embedding provider
    self.metrics.cache_misses.inc();
    let embedding_result = self.provider.embed(query_text).await?;
    let query_vector = embedding_result.vector.clone();

    // Store in cache
    self.embedding_cache.insert(cache_key, query_vector.clone()).await;

    self.search_by_vector(topic, &query_vector, k, filters).await
}
```

### Files to Modify

| File | Change |
|------|--------|
| `crates/chronik-columnar/src/vector_index.rs` | Add `QueryEmbeddingCache` field to `VectorSearchService`, cache check in `search_by_text()` |
| `crates/chronik-columnar/src/vector_index.rs` | Add cache check in hybrid search path too (`search_hybrid()`) |
| `crates/chronik-monitoring/src/unified_metrics.rs` | Add `chronik_embedding_cache_hits_total`, `chronik_embedding_cache_misses_total`, `chronik_embedding_cache_size` metrics |
| `crates/chronik-server/src/unified_api/vector_handler.rs` | Pass cache to `VectorSearchService` constructor |

### Configuration

| Env Var | Default | Description |
|---------|---------|-------------|
| `CHRONIK_EMBEDDING_CACHE_ENABLED` | `true` | Enable/disable query embedding cache |
| `CHRONIK_EMBEDDING_CACHE_MAX_ENTRIES` | `10000` | Maximum cached embeddings |
| `CHRONIK_EMBEDDING_CACHE_TTL_SECS` | `3600` | TTL before eviction |

### Expected Results

| Scenario | Before | After |
|----------|--------|-------|
| First query (cache miss) | 372ms p50 | 372ms p50 (unchanged) |
| Repeated query (cache hit) | 372ms p50 | **1-5ms p50** |
| Hybrid search (cache hit) | 376ms p50 | **3-8ms p50** |
| Orchestrator hybrid (cache hit) | 645ms p50 | **7-12ms p50** |

### Prometheus Metrics

- `chronik_embedding_cache_hits_total` (counter)
- `chronik_embedding_cache_misses_total` (counter)
- `chronik_embedding_cache_evictions_total` (counter)
- `chronik_embedding_cache_size` (gauge) — current number of cached embeddings
- `chronik_embedding_cache_hit_ratio` (derived) — hits / (hits + misses)

### Verification

1. Run the same 37-query scale test twice
2. First run: all cache misses, latency ~372ms p50 (same as today)
3. Second run: all cache hits, vector latency should drop to 1-5ms
4. Verify via Prometheus that `cache_hits_total` matches iteration count on second run

**Effort**: ~2-3 hours
**Risk**: Low — additive change, doesn't affect existing behavior

---

## SV-2 — Large-Scale Validation (16.6M Documents)

**Goal**: Validate Chronik's text search, SQL, and vector search at 1M-16M documents using real-world operational logs.

### Dataset: Loghub-2.0 Thunderbird

| Property | Value |
|----------|-------|
| **Source** | Sandia National Labs Thunderbird supercomputer |
| **Records** | 16,601,745 log lines |
| **Size** | ~1.2 GB uncompressed |
| **Content** | Timestamps, severity, component, free-text messages, labeled anomalies |
| **Download** | [Zenodo](https://zenodo.org/records/8275861) — direct download, zero auth |
| **Repository** | [logpai/loghub-2.0](https://github.com/logpai/loghub-2.0) |

### Why This Dataset

- Real operational logs from a 9,024-processor supercomputer
- Maps directly to Chronik's core use case (log search and analysis)
- Rich variety: error messages, warnings, system events, component failures
- Labeled anomalies allow relevance evaluation (search for "failure" should find anomaly-labeled entries)
- Large enough to stress Tantivy index, DataFusion Parquet, and HNSW at scale

### Test Plan

#### SV-2a — Text Search + SQL at 16.6M (no embeddings)

Load all 16.6M logs into a single topic. No embeddings — this validates Tantivy and DataFusion at scale without OpenAI cost.

**Ingestion**:
- K8s Job with Python Kafka producer
- Expected throughput: ~3,500 msg/s → ~79 minutes for 16.6M messages
- Alternative: Batch loader that reads Thunderbird log file and produces in bulk (~10K msg/s target)

**Queries to benchmark** (20 iterations each):

| Category | Queries |
|----------|---------|
| **Simple text** | `error`, `warning`, `failure`, `timeout`, `kernel` |
| **Multi-term** | `network interface down`, `disk I/O error`, `memory allocation failed` |
| **Boolean** | `error AND kernel`, `warning AND network AND interface` |
| **Broad** | `error` (expect thousands of hits), `warning` with size=100 |
| **SQL COUNT** | `SELECT COUNT(*) FROM thunderbird_hot` |
| **SQL GROUP BY** | `SELECT severity, COUNT(*) FROM thunderbird_hot GROUP BY severity` |
| **SQL WHERE** | `SELECT * FROM thunderbird_hot WHERE severity = 'ERROR' LIMIT 50` |
| **SQL time range** | `SELECT * FROM thunderbird WHERE timestamp > X LIMIT 20` |

**Success criteria**:
- Text search p50 < 10ms at 16.6M docs (Tantivy should handle this easily)
- SQL COUNT p50 < 50ms at 16.6M rows
- SQL GROUP BY p50 < 100ms
- Zero errors, zero panics

#### SV-2b — Vector Search at 1M (subset with embeddings)

Embed 1M messages from the Thunderbird dataset. At ~30 tokens per log message:

| Metric | Estimate |
|--------|----------|
| Messages to embed | 1,000,000 |
| Tokens | ~30M |
| OpenAI cost | ~$0.60 (at $0.02/1M tokens) |
| Embedding time | ~8-12 hours (background WalIndexer) |
| Vectors indexed | ~1,000,000 x 1536d |
| HNSW index memory | ~6 GB (1M x 1536 x 4 bytes) |

**Queries to benchmark** (20 iterations each):

| Query | Type |
|-------|------|
| `disk failure causing data loss` | Semantic search on ops logs |
| `network connectivity between nodes` | Infrastructure failure |
| `memory allocation kernel panic` | System crash |
| `process scheduling delay` | Performance degradation |
| `hardware error correctable ECC` | Hardware fault detection |

**Success criteria**:
- HNSW search p50 < 10ms at 1M vectors (without embedding, cache hit)
- HNSW search p50 < 500ms at 1M vectors (with embedding, cache miss)
- Vector index build doesn't crash or OOM
- Tantivy text search unaffected by vector index size

#### SV-2c — Hybrid + Orchestrator at Scale

After both text and vector indexes are populated:

| Query | Mode |
|-------|------|
| `kernel panic memory fault` | Hybrid (text + vector) |
| `network interface link down` | Hybrid (text + vector) |
| Multi-topic: thunderbird + app-logs | Orchestrator 2-topic text |
| Multi-topic: thunderbird + app-logs | Orchestrator 2-topic hybrid |

**Success criteria**:
- Hybrid p50 < 500ms (cache miss) / < 10ms (cache hit)
- Orchestrator adds < 5ms overhead per topic for text
- No degradation in existing 30K dataset queries

### Alternative Datasets

If Thunderbird proves unsuitable, these are strong alternatives:

| Dataset | Records | Source | Best For |
|---------|---------|--------|----------|
| **HDFS Logs** (Loghub-2.0) | 11.2M | Zenodo (same download) | Hadoop-style ops logs |
| **BGL Supercomputer** (Loghub-2.0) | 4.6M | Zenodo (same download) | Labeled anomaly detection |
| **MS MARCO Passages** | 8.8M | Microsoft (with terms) | Gold standard IR benchmark with relevance labels |
| **ArXiv Metadata** | 2.4M | Kaggle | Structured fields (SQL) + abstracts (vector) |

### K8s Manifests

| File | Purpose |
|------|---------|
| `tests/k8s-perf/90-thunderbird-loader-configmap.yaml` | Python script to parse and produce Thunderbird logs |
| `tests/k8s-perf/91-thunderbird-loader-job.yaml` | K8s Job for ingestion |
| `tests/k8s-perf/92-thunderbird-perf-configmap.yaml` | Scale test script for 16.6M docs |
| `tests/k8s-perf/93-thunderbird-perf-job.yaml` | K8s Job for perf test |

### Preparation

```bash
# Download Thunderbird logs from Zenodo
wget https://zenodo.org/records/8275861/files/Thunderbird.tar.gz
tar xzf Thunderbird.tar.gz

# Parse format: each line is a structured log entry
# Fields: Label, Timestamp, Date, User, Month, Day, Time, Location, Component, PID, Content
# Example: "- 1131562298 2005.11.09 R02-M1-N0-C:J12-U01 Nov 9 15:11:38 ..."
```

**Effort**: ~3-4 hours (download, parse, load, test)
**Risk**: Medium — 16.6M docs may require tuning Tantivy commit interval and HotDataBuffer size. HNSW at 1M vectors needs ~6GB RAM.

---

## SV-3 — Multi-Node Cluster Deployment

**Goal**: Deploy Chronik as a 3-node cluster on the k8s bare metal cluster and validate multi-node query performance.

### Current State

The Phase 9 tests used a single standalone pod. Chronik supports multi-node cluster mode with:
- Raft consensus for metadata replication
- Partition assignment across nodes
- WAL replication for data durability
- Automatic leader election and failover

### What Multi-Node Gives Us Today

With 3 nodes, each owning different partitions:

```
                    Load Balancer (k8s Service)
                    ┌──────┼──────┐
                    v      v      v
                 Node 1  Node 2  Node 3
                 ┌────┐  ┌────┐  ┌────┐
  app-logs:      |P0,P1|  |P2,P3|  |P4,P5|
  support:       |P0,P1|  |P2,P3|  |P4,P5|
  system:        |P0,P1|  |P2,P3|  |P4,P5|
                 └────┘  └────┘  └────┘
```

- **3x ingestion throughput** — Producers spread across partitions on different nodes
- **3x query throughput** — Load balancer distributes queries across nodes, each serves its local partitions
- **Fault tolerance** — Raft replication means any 1 node can die without data loss
- **Partition-local search** — Each node's Tantivy/HNSW/Parquet indexes cover only its partitions

### Deployment Plan

#### Step 1: Build Cluster Image

```bash
# Same image as standalone, cluster mode is config-driven
docker build -t chronik-server:v2.5.0-cluster .
# Push to all 3 nodes
for node in dell-1 dell-2 dell-3; do
  docker save chronik-server:v2.5.0-cluster | ssh $node docker load
done
```

#### Step 2: Create Cluster K8s Manifests

| File | Purpose |
|------|---------|
| `tests/k8s-perf/95-chronik-cluster-configmap.yaml` | Cluster TOML config (3 nodes, peers, Raft addresses) |
| `tests/k8s-perf/96-chronik-cluster-statefulset.yaml` | StatefulSet with 3 replicas, each getting unique node-id |
| `tests/k8s-perf/97-chronik-cluster-service.yaml` | Headless service for inter-node Raft + client-facing service |

**StatefulSet approach**:
```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: chronik-cluster
  namespace: chronik-perf
spec:
  replicas: 3
  serviceName: chronik-cluster-headless
  template:
    spec:
      containers:
        - name: chronik
          image: chronik-server:v2.5.0-cluster
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: metadata.labels['apps.kubernetes.io/pod-index']
          command:
            - chronik-server
            - start
            - --config
            - /config/cluster.toml
            - --node-id
            - "$(NODE_ID)"
          ports:
            - containerPort: 9092   # Kafka
            - containerPort: 6092   # Unified API
            - containerPort: 9291   # WAL replication
            - containerPort: 5001   # Raft consensus
```

#### Step 3: Data Loading

Load the same 30K dataset (or Thunderbird 16.6M) into the cluster. Messages auto-distribute across partitions/nodes.

#### Step 4: Benchmark

| Test | Purpose | Expected |
|------|---------|----------|
| Text search via each node | Verify per-node search works | Same latency as standalone (~3ms) |
| Text search via load balancer | Measure distributed throughput | ~3x throughput (3 nodes) |
| Vector search via each node | Verify per-node vectors | Same latency (~372ms) |
| Failover: kill 1 node, query others | Verify availability | Queries to surviving nodes succeed |
| Recovery: restart killed node | Verify data recovery | Node catches up via Raft/WAL |
| Ingest during queries | Verify read/write isolation | Query latency unaffected |

### Success Criteria

- 3 Chronik nodes in Raft cluster, all healthy
- Leader election completes in < 5 seconds
- Query latency per-node matches standalone (no Raft overhead on reads)
- Load-balanced throughput ~3x single node
- Killing 1 node doesn't affect queries to other 2 nodes
- Killed node recovers automatically on restart

**Effort**: ~3-4 hours (manifests, deploy, test)
**Risk**: Medium — StatefulSet with Raft requires careful peer discovery. Pod DNS must resolve before Raft can form quorum.

---

## SV-4 — Cross-Node Query Fan-Out (Future)

**Goal**: Enable any node to return complete results across all partitions, even those owned by other nodes.

**Status**: Design only. Implementation deferred until SV-3 validates multi-node operation.

### The Gap

Currently, a query hitting Node 1 only searches Node 1's partitions. For complete results across all partitions of a topic, you'd need to query all 3 nodes separately. There is no scatter-gather across nodes for a single topic's search.

### Design

Extend the Query Orchestrator's `QueryPlanner` to be partition-aware:

```rust
pub enum ExecutionNode {
    // Existing: local execution
    Text { topic: String, query: String, k: usize },
    Vector { topic: String, query: String, k: usize },

    // New: remote execution
    RemoteText { node_id: u64, topic: String, partitions: Vec<i32>, query: String, k: usize },
    RemoteVector { node_id: u64, topic: String, partitions: Vec<i32>, query: String, k: usize },
}
```

**Planning logic**:
1. For each topic in the query, get partition assignments from metadata
2. Partitions owned locally → `Text`/`Vector` nodes (same as today)
3. Partitions owned by other nodes → `RemoteText`/`RemoteVector` nodes
4. Execute all in parallel (local + remote)
5. Merge results via RRF (same as today)

**Remote execution**: HTTP call to `/_search` or `/_vector/{topic}/search` on the owning node's Unified API. Each node already exposes these endpoints.

**Latency impact**: Adds 1-2ms network round-trip per remote node (cluster-internal). With 3 nodes, worst case is 2 remote calls adding ~2-4ms total to a text query.

### Files to Modify

| File | Change |
|------|--------|
| `crates/chronik-query/src/plan.rs` | Add `RemoteText`/`RemoteVector` variants to `ExecutionNode` |
| `crates/chronik-query/src/orchestrator.rs` | Execute remote nodes via HTTP client |
| `crates/chronik-query/src/capabilities.rs` | Include partition-to-node mapping |
| `crates/chronik-server/src/unified_api/mod.rs` | Expose partition filtering on search endpoints |

**Effort**: ~1-2 days
**Risk**: Medium — Requires partition-aware query filtering on search endpoints. Need to handle node failures gracefully (return partial results).

---

## Summary

| Phase | What | Impact | Effort | Risk |
|-------|------|--------|--------|------|
| **SV-1** | Query embedding cache | Vector queries 372ms → 1-5ms on cache hit | 2-3 hours | Low |
| **SV-2a** | Thunderbird 16.6M text+SQL | Validate Tantivy/DataFusion at real scale | 3-4 hours | Medium |
| **SV-2b** | Thunderbird 1M vectors | Validate HNSW at 1M vectors, ~$0.60 OpenAI | 8-12 hours (embedding time) | Medium |
| **SV-2c** | Hybrid + orchestrator at scale | End-to-end validation at 16.6M | 1-2 hours (after SV-2a+b) | Low |
| **SV-3** | 3-node cluster on k8s | Multi-node validation, 3x throughput, failover | 3-4 hours | Medium |
| **SV-4** | Cross-node query fan-out | Complete results from any node | 1-2 days | Medium |

### Execution Order

```
SV-1 (cache) ──────────────────► immediate, biggest user-facing impact
SV-2a (16.6M text+SQL) ────────► parallel with SV-1
SV-3 (cluster deploy) ─────────► after SV-1, validates multi-node
SV-2b (1M vectors) ────────────► after SV-1 (cache makes testing faster)
SV-2c (hybrid at scale) ───────► after SV-2a + SV-2b
SV-4 (cross-node fan-out) ─────► after SV-3, design-only until validated
```

### Market Position After This Roadmap

| Weakness (today) | After | Market comparison |
|-------------------|-------|-------------------|
| Vector queries always hit embedding API | Cache hit: 1-5ms, matching dedicated vector DBs | Competitive with Pinecone (40-50ms) and Qdrant (5-15ms) |
| Unvalidated beyond 150K docs | Validated at 16.6M text, 1M vectors | Comparable to published Elasticsearch benchmarks |
| Single-pod query serving | 3-node cluster with 3x throughput | Matches Elasticsearch multi-shard architecture |
| No cross-node query merging | Designed, ready for implementation | Closes gap with Elasticsearch cross-shard search |
