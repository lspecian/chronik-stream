# Chronik vs Market — Performance Comparison

How Chronik's search and query performance compares to dedicated systems in each category. All Chronik numbers are from validated k8s bare metal testing (see [SEARCH_RELEVANCE_PERFORMANCE.md](SEARCH_RELEVANCE_PERFORMANCE.md)).

---

## Text Search (BM25)

| System | p50 Latency | Throughput | Notes |
|--------|------------|------------|-------|
| **Chronik (Tantivy)** | **3.4ms** | **456 q/s** (single pod, 14% CPU) | 30K docs, Rust-native |
| Elasticsearch (Lucene) | 5-50ms | Varies with cluster size | Industry standard, JVM-based |
| OpenSearch (Lucene) | ~10-40ms | ~11% slower than ES on vector workloads | AWS fork of Elasticsearch |
| Meilisearch | ~5-20ms | Optimized for small-medium datasets | Rust-based, typo-tolerant |

**Verdict**: Best-in-class. Tantivy is a Rust rewrite of Lucene, known to be 2-5x faster for equivalent workloads. 3.4ms p50 at 30K documents with 456 q/s from a single pod at 14% CPU utilization leaves massive headroom for scaling.

---

## Vector Search (HNSW)

| System | HNSW Lookup | End-to-End with OpenAI Embedding | Notes |
|--------|------------|----------------------------------|-------|
| **Chronik** | **1-5ms** | **372ms p50** | 132K vectors, 1536d, includes embedding API call |
| Pinecone | N/A (managed) | 40-50ms p95 (pre-computed vectors only) | Dedicated managed vector DB |
| Weaviate | ~5-20ms | 50-70ms p95 (pre-computed vectors only) | Open-source, HNSW-based |
| Qdrant | ~5-15ms | sub-100ms (pre-computed vectors only) | Rust-based, dedicated vector DB |
| Elasticsearch kNN | 5-50ms (HNSW + quantization) | Similar to Chronik with embedding module | Requires separate vector index setup |
| Milvus | ~10-50ms | Depends on embedding pipeline | Distributed vector DB |

**Important context**: Dedicated vector DBs (Pinecone, Qdrant, Weaviate) quote latency **without embedding computation** — they assume pre-computed vectors. When the OpenAI API call is included (~200-500ms), every system lands in the 300-600ms range. Chronik's raw HNSW lookup at 1-5ms is competitive with all dedicated vector databases.

**Verdict**: Competitive. The HNSW layer itself matches dedicated vector DBs. Total latency is dominated by embedding API round-trip, which is identical regardless of which system you use.

---

## SQL on Streams

| System | Query Latency | Notes |
|--------|-------------|-------|
| **Chronik (DataFusion)** | **5.9ms p50** | COUNT/SELECT on hot (Arrow) + cold (Parquet) |
| ksqlDB pull queries | 100ms-1s | Kafka-native, designed for streaming transforms |
| ClickHouse | 50-100ms | Dedicated OLAP, optimized for complex aggregations |
| Trino/Presto on Kafka | 500ms-seconds | Batch-oriented, not designed for interactive queries |
| Real-time analytics DBs | 50-100ms | Complex queries with filters/aggregates/joins |

**Verdict**: Significantly faster than alternatives. 5.9ms for interactive SQL on a Kafka topic is 17-170x faster than ksqlDB pull queries. The HotDataBuffer (Arrow in-memory) gives Chronik a structural advantage for point queries that no Kafka-to-warehouse pipeline can match.

---

## Hybrid Search (Text + Vector Fusion)

| System | p50 Latency | Fusion Method | Notes |
|--------|------------|---------------|-------|
| **Chronik** | **376ms** | RRF (Reciprocal Rank Fusion) | Includes OpenAI embedding call |
| Elasticsearch 8.x | ~50-200ms | RRF (8.8+) | Requires separate kNN index, no embedding included |
| Weaviate | ~100-500ms | BM25 + vector fusion | Depends on embedding backend |
| Vespa | ~50-200ms | Configurable ranking | Enterprise search platform |

**Verdict**: Competitive when embedding latency is factored in. Chronik adds < 100ms overhead for the text+vector fusion itself. The rest is OpenAI API latency, which applies equally to any system using cloud embeddings.

---

## Multi-Topic Query Orchestration

| System | Capability | Latency |
|--------|-----------|---------|
| **Chronik** | **3-topic text fan-out** | **7ms** |
| **Chronik** | **3-topic hybrid (6 backends)** | **645ms p50** |
| Elasticsearch | Cross-index search | 10-100ms (text only) |
| Weaviate | Single-collection only | N/A |
| Pinecone | Single-namespace only | N/A |

**Verdict**: Unique capability. No competing system offers multi-topic, multi-backend (text + vector + SQL) fan-out with RRF fusion and configurable ranking profiles in a single request. Elasticsearch can search across indices but doesn't combine text + vector + SQL backends.

---

## The Real Comparison: Architecture

The most important benchmark isn't individual query latency — it's what Chronik replaces:

```
Traditional Stack                              Chronik
─────────────────                              ───────
Kafka (message broker)             ─┐
  + Kafka Connect (pipelines)       │
  + Elasticsearch (text search)     │           Single binary
  + Pinecone/Weaviate (vectors)     ├──►        3.4ms text search
  + Data warehouse (SQL)            │           372ms vector search
  + ETL connectors                  │           5.9ms SQL queries
  + Monitoring for each system    ─┘           7ms multi-topic orchestration
```

### What the traditional pipeline costs you

| Concern | Traditional Stack | Chronik |
|---------|-------------------|---------|
| **Data propagation delay** | Minutes (Kafka → Connect → ES/VectorDB) | 30-90s (WalIndexer background) |
| **Systems to operate** | 5-7 (Kafka, ES, vector DB, warehouse, connectors, monitoring) | 1 binary |
| **Data consistency** | Eventual (each system has its own lag) | Single source of truth (WAL) |
| **Failure modes** | Each connector/system can fail independently | Single process, WAL-backed recovery |
| **Infrastructure cost** | ES cluster + vector DB + warehouse + Kafka | Single pod (4 CPU, 8 GB) |
| **Embedding infrastructure** | Separate embedding pipeline + GPU cluster | Built-in (OpenAI API or self-hosted) |
| **Query correlation** | Manual (search ES, then query warehouse, then check vector DB) | Single `/_query` request across all backends |

### Cost comparison (rough estimate for 30K messages/3 topics)

| Component | Traditional | Chronik |
|-----------|------------|---------|
| Kafka | $200-500/mo (managed) | Included |
| Elasticsearch | $300-800/mo (3-node) | Included |
| Vector DB | $70-200/mo (Pinecone starter) | Included |
| Data warehouse | $100-400/mo (min) | Included |
| Connectors/ETL | $50-200/mo | Not needed |
| OpenAI embeddings | $0.20/30K messages | $0.20/30K messages |
| **Total** | **$720-2,100/mo** | **Single pod + $0.20** |

---

## Where Chronik is Weaker (Honest Assessment)

### 1. Pure vector throughput under extreme concurrency
Dedicated vector DBs like Pinecone handle thousands of concurrent vector queries because vectors are pre-indexed and served from optimized storage. Chronik computes embeddings at query time, so it's bottlenecked by the embedding API under heavy concurrent vector load. At 500 VUs, dedicated vector DBs win on raw throughput.

### 2. Dataset scale validation
Current tests are at 30K-150K documents. Elasticsearch and Pinecone publish benchmarks at 1M-50M+ vectors. Chronik hasn't been validated at that scale yet — though both Tantivy and HNSW are proven to scale well in standalone benchmarks.

### 3. Multi-node query distribution
Elasticsearch distributes search across shards on multiple nodes automatically. Chronik is single-pod today for query serving. Horizontal query scaling (read replicas, shard distribution) is the next frontier.

### 4. Ecosystem and tooling
Elasticsearch has Kibana, Logstash, Beats, and decades of ecosystem. Pinecone has managed infrastructure and client libraries in every language. Chronik is a single binary with a REST API — powerful but less ecosystem support.

---

## Summary

| Category | vs Market | Detail |
|----------|-----------|--------|
| **Text Search** | Best-in-class | 3.4ms p50, Tantivy is 2-5x faster than Lucene |
| **Vector Search** | Competitive | 1-5ms HNSW lookup matches dedicated vector DBs |
| **SQL on Streams** | Significantly faster | 5.9ms vs 100ms+ (ksqlDB) or seconds (Trino) |
| **Hybrid Search** | Competitive | < 100ms fusion overhead, embedding API is the equalizer |
| **Multi-Backend Orchestration** | Unique | No competitor does text+vector+SQL fan-out in one request |
| **Architectural Simplicity** | Major advantage | 1 binary replaces 5-7 systems |
| **Scale (1M+ vectors)** | Unvalidated | Needs testing at larger scale |
| **Ecosystem** | Weaker | No Kibana equivalent, fewer client libraries |

**Bottom line**: For a single binary that replaces Kafka + Elasticsearch + a vector database + a data warehouse + all the connectors between them, each individual capability is competitive with — and in some cases faster than — the dedicated tool it replaces. The value proposition isn't "faster than Pinecone at vector search" — it's **"one system instead of five, and none of the individual capabilities are compromised."**
