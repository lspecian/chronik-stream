# Chronik Relevance Engine Roadmap

**Project**: Unified Query Orchestration + Multi-Phase Ranking + ML Relevance Lifecycle
**Supersedes**: ROADMAP-9.md (retained as historical reference)
**Informed By**: [RESEARCH_QUERY_RANKING.md](RESEARCH_QUERY_RANKING.md)
**Phases**: 9 (Query Orchestrator), 10 (Reranking + ML), 11 (Policy + Governance)

---

## Vision

Transform Chronik from a streaming platform that **retrieves data** into a platform that **understands what matters** — covering the full ML relevance lifecycle from rule-based scoring through production ML ranking, in a single binary.

```
Today (8-15 systems):
  Kafka -> Flink -> Elasticsearch -> Vector DB -> Feature Store -> ML Model -> Serving -> Monitoring

Chronik (1 system):
  Produce -> Store -> Search -> Rank -> Serve
  (one binary, one API, one operational surface)
```

No other platform combines Kafka-compatible streaming with multi-phase ranking. Confluent connects to other systems. Vespa requires external streaming. Chronik does both.

---

## Architecture: Three-Phase Ranking

Inspired by Vespa's three-phase model, adapted for Chronik's streaming-first architecture:

```
POST /_query
    |
    v
[Phase 0: Query Understanding]  (Phase 11)
    LLM query expansion, intent classification
    |
    v
[Phase 1: Retrieval]  (Phase 9 - done)
    Parallel fan-out to topic backends:
    +-- BM25 (Tantivy) -----------> candidates
    +-- Vector (HNSW) ------------> candidates
    +-- SQL (DataFusion) ---------> candidates
    +-- Fetch (WAL) --------------> candidates
    |
    v
[Phase 2: First-Pass Ranking]  (Phase 9 - done)
    +-- RRF Merger (fuse multi-backend rank lists)
    +-- RuleRanker (JSON weight profiles)
    +-- Expression Engine (scoring expressions)  (Phase 10)
    +-- GBDT Model (XGBoost/LightGBM files)  (Phase 10)
    |
    v
[Phase 3: Reranking]  (Phase 10)
    Pluggable, expensive second-pass:
    +-- Cross-encoder (Cohere Rerank, Jina, VoyageAI)
    +-- ColBERT (late-interaction, self-hosted)
    +-- ONNX model (local inference)
    +-- LLM reranker (OpenAI, Anthropic, external)
    |
    v
[Policy Overlay]  (Phase 11)
    +-- Access control (per-topic, per-user)
    +-- Boosting / burying / pinning rules
    +-- Content filtering (compliance, PII)
    +-- Audit logging
    |
    v
[Feature Logging]  (Phase 10)
    Every query logs: features, scores, explanations
    -> Training data for future ML models
    |
    v
Response (ranked results + explanations + stats)
```

---

## Phase 9 — Query Orchestrator + First-Pass Ranking ✅ COMPLETE

**Goal**: Ship the `POST /_query` endpoint with multi-topic parallel retrieval, RRF-based hybrid search, rule-based ranking profiles, and feature logging.

**Scope**: Everything a user needs for production-quality search WITHOUT ML models.

**Status**: All modules implemented and tested. 58 tests (45 unit + 13 integration). E2E demo at `examples/log-analysis/demo.sh` passes all 12 scenarios.

### 9.1 Core Types & Crate Setup ✅

**New modules in `crates/chronik-query/src/`**:

| Module | Purpose |
|--------|---------|
| `types.rs` | Request/response types for `/_query` |
| `candidate.rs` | Normalized result format + deduplication |
| `capabilities.rs` | Per-topic capability detection + caching |
| `plan.rs` | QueryPlan + ExecutionNode construction |
| `orchestrator.rs` | Parallel execution of plan nodes |
| `rrf.rs` | Reciprocal Rank Fusion |
| `ranker.rs` | Ranker trait + RuleRanker |
| `profiles.rs` | JSON/YAML ranking profiles + hot reload |
| `features.rs` | Feature extraction + feature logging |

**Existing code to reuse** (not replace):
- `query_executor.rs` (367 LOC) — Tantivy query execution
- `translator.rs` (1,262 LOC) — QueryDSL -> Tantivy translation
- `aggregation.rs` (399 LOC) — Time-windowed aggregation

### 9.2 Unified Query API ✅

**`POST /_query`** — single endpoint for multi-topic, multi-backend queries:

```json
{
  "sources": [
    {"topic": "logs", "modes": ["text", "vector"]},
    {"topic": "orders", "modes": ["sql"]},
    {"topic": "events", "modes": ["fetch"]}
  ],
  "q": {
    "text": "payment failed",
    "semantic": "card declined",
    "sql": "SELECT * FROM orders WHERE status = 'failed' LIMIT 50"
  },
  "filters": {
    "timestamp_gte": "2026-02-01T00:00:00Z",
    "timestamp_lte": "2026-02-22T23:59:59Z"
  },
  "k": 50,
  "rank": {
    "profile": "default"
  },
  "result_format": "merged"
}
```

**Response**:

```json
{
  "query_id": "q-abc123",
  "results": [
    {
      "topic": "logs",
      "partition": 2,
      "offset": 91827,
      "final_score": 0.92,
      "features": {
        "text_rank": 1,
        "vector_rank": 3,
        "rrf_score": 0.033,
        "freshness": 0.88,
        "source_count": 2
      },
      "explanation": "Matched by text (rank 1) and vector (rank 3). Boosted by freshness (0.88) and multi-source match."
    }
  ],
  "stats": {
    "candidates": 240,
    "ranked": 50,
    "latency_ms": 42,
    "backends": [
      {"backend": "text", "topic": "logs", "candidates": 100, "latency_ms": 8, "timed_out": false},
      {"backend": "vector", "topic": "logs", "candidates": 100, "latency_ms": 22, "timed_out": false},
      {"backend": "sql", "topic": "orders", "candidates": 40, "latency_ms": 35, "timed_out": false}
    ]
  }
}
```

**`GET /_query/capabilities`** — discover what each topic supports:

```json
{
  "topics": {
    "logs": {"text_search": true, "vector_search": true, "sql_query": false, "fetch": true},
    "orders": {"text_search": false, "vector_search": false, "sql_query": true, "fetch": true}
  }
}
```

### 9.3 Topic Capabilities Detection ✅

`CapabilityDetector` trait checks backend availability per topic:
- Tantivy index exists? -> `text_search: true`
- HNSW index + embedding config? -> `vector_search: true`
- Parquet files or HotBuffer registered? -> `sql_query: true`
- Topic exists in metadata? -> `fetch: true`

Cached in `CapabilitiesCache` with 60s TTL.

### 9.4 Query Plan Engine ✅

`QueryPlanner` builds a `QueryPlan` from the request:
1. For each source/topic, check capabilities
2. For each supported mode, create an `ExecutionNode`
3. Skip unsupported modes with warning (don't fail)
4. Set per-node timeout (default 5s)

`QueryOrchestrator` executes the plan:
1. Spawn all nodes as `tokio::spawn` tasks
2. Collect with timeout (`tokio::time::timeout`)
3. Convert backend results to `Candidate` via adapters
4. Return partial results if some backends timeout

**Adapters**:
- Tantivy `SearchHit` -> `Candidate`
- `VectorSearchResult` -> `Candidate`
- DataFusion `RecordBatch` row -> `Candidate`
- WAL fetch record -> `Candidate`

### 9.5 Reciprocal Rank Fusion (RRF) ✅

Merge results when multiple backends return candidates for the same topic:

```
RRF_score(doc) = SUM( 1 / (k + rank_i) )   where k = 60
```

- Uses only **rank positions**, not raw scores
- Documents found by multiple backends get naturally boosted
- Proven in production: Elasticsearch 8.x, Weaviate, Pinecone
- Works for both per-topic hybrid merge AND cross-topic merge

**Result format options** (per request):
- `"merged"` — single list across all topics, ranked by RRF + profile
- `"grouped"` — results partitioned by topic, independently ranked

### 9.6 Rule-Based Ranking (JSON Profiles) ✅

Ranking profiles define how candidates are scored after RRF:

```json
{
  "name": "default",
  "description": "Balanced ranking with freshness boost",
  "weights": {
    "rrf_score": 0.6,
    "freshness": 0.2,
    "source_count": 0.2
  },
  "boosts": [
    {"condition": "source_count > 1", "boost": 1.2, "description": "Multi-match boost"}
  ]
}
```

**Built-in profiles**:
- `default` — balanced weights
- `freshness` — heavy time-decay weighting (logs, alerts)
- `relevance` — heavy text/vector weight (search, RAG)

**Hot reload**: Poll `$CHRONIK_DATA_DIR/profiles/` every 30s or on filesystem notification.

### 9.7 Feature Extraction ✅

Features computed per candidate at query time:

| Feature | Type | Description |
|---------|------|-------------|
| `text_rank` | int | Position in Tantivy results (0-indexed) |
| `vector_rank` | int | Position in HNSW results |
| `sql_rank` | int | Position in SQL results |
| `rrf_score` | float | Combined RRF score |
| `raw_text_score` | float | BM25 score from Tantivy |
| `raw_vector_score` | float | Cosine distance from HNSW |
| `freshness` | float | `1.0 / (1.0 + age_hours)` |
| `source_count` | int | How many backends found this candidate |
| `message_size` | int | Payload size in bytes |

Features are included in every response and logged for training data.

### 9.8 Feature Logging (Deferred to Phase 10)

Every `/_query` request logs a structured record:

```json
{
  "query_id": "q-abc123",
  "timestamp": "2026-02-22T10:15:00Z",
  "request": { "sources": [...], "q": {...}, "k": 50, "profile": "default" },
  "results": [
    {
      "topic": "logs", "partition": 2, "offset": 91827,
      "features": {"text_rank": 1, "vector_rank": 3, "rrf_score": 0.033, "freshness": 0.88},
      "final_score": 0.92,
      "position": 0
    }
  ],
  "stats": { "candidates": 240, "ranked": 50, "latency_ms": 42 }
}
```

**Storage**: Written to a dedicated Chronik topic (`_chronik_query_logs`) via the internal produce path. This makes query logs:
- Durable (WAL-backed)
- Searchable (via Tantivy, if indexing is enabled on the topic)
- Queryable (via SQL, if columnar is enabled)
- Exportable (via Kafka consumer protocol)

**Purpose**: Training data for future LTR models. Users can consume `_chronik_query_logs` with any Kafka consumer, extract features, train XGBoost models offline, and deploy them back.

### 9.9 Prometheus Metrics ✅

New metrics via `chronik-monitoring`:

| Metric | Type | Labels |
|--------|------|--------|
| `chronik_query_requests_total` | counter | profile |
| `chronik_query_latency_seconds` | histogram | - |
| `chronik_query_candidates_total` | histogram | - |
| `chronik_query_backend_latency_seconds` | histogram | backend, topic |
| `chronik_query_backend_timeouts_total` | counter | backend, topic |
| `chronik_query_errors_total` | counter | error_type |
| `chronik_query_features_logged_total` | counter | - |

### 9.10 Integration Tests ✅

| Test | What It Verifies |
|------|-----------------|
| Single-topic, single-mode | Text-only query returns ranked results |
| Single-topic, hybrid | Text + vector with RRF merge |
| Multi-topic | 3 topics queried in parallel |
| Timeout handling | One slow backend, others return partial results |
| Missing capability | Vector requested on non-vector topic (skip, don't fail) |
| Profile switching | Default vs freshness vs relevance produce different orderings |
| Feature logging | Query log record written to `_chronik_query_logs` |
| Capabilities endpoint | `GET /_query/capabilities` returns correct data |
| Merged vs grouped | Both result formats work correctly |

---

## Phase 10 — Reranking + ML Model Integration

**Goal**: Pluggable second-pass reranking with external services and local model files. Expression-based ranking language.

### 10.1 Reranker Trait & Provider Architecture

```rust
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Rerank candidates using this provider.
    async fn rerank(
        &self,
        query: &str,
        candidates: Vec<Candidate>,
        top_n: usize,
    ) -> Result<Vec<Candidate>>;

    /// Provider name (for metrics/logging)
    fn name(&self) -> &str;
}
```

### 10.2 Pluggable Reranking Providers

| Provider | Type | Latency | Notes |
|----------|------|---------|-------|
| `CohereReranker` | API | 100-500ms | `cohere-rerank-3.5`, best quality/cost ratio |
| `JinaReranker` | API | 100ms-7s | Listwise, variable by doc length |
| `VoyageReranker` | API | 100-300ms | Good accuracy, competitive pricing |
| `OnnxReranker` | Local | 20-100ms | Load ONNX cross-encoder model files |
| `LlmReranker` | API | 1-7s | Via existing `EmbeddingProvider` infra |
| `GbdtReranker` | Local | 1-10ms | Load XGBoost/LightGBM model files |
| `ColbertReranker` | Local | 20-100ms | Late-interaction, pre-computed doc embeddings |

**Configuration via ranking profiles**:

```json
{
  "name": "high_quality",
  "weights": { "rrf_score": 0.8, "freshness": 0.2 },
  "rerank": {
    "provider": "cohere",
    "model": "rerank-english-v3.5",
    "top_n": 20,
    "api_key_env": "COHERE_API_KEY"
  }
}
```

### 10.3 GBDT Model Loading

Support loading XGBoost and LightGBM model files for fast local ranking:

```json
{
  "name": "ecommerce",
  "rerank": {
    "provider": "xgboost",
    "model_path": "models/product_ranker.json",
    "top_n": 100,
    "features": ["rrf_score", "freshness", "popularity", "price", "in_stock"]
  }
}
```

Model files stored in `$CHRONIK_DATA_DIR/models/`. Hot-reloaded on change.

### 10.4 Expression Engine

Simple expression language for ranking, growing toward Vespa-style:

**Step 1**: Arithmetic expressions with feature references:
```
rrf_score * 0.6 + freshness * 0.2 + source_count * 0.1 + popularity * 0.1
```

**Step 2**: Conditionals and function calls:
```
if(source_count > 1,
    rrf_score * 0.7 + freshness * 0.3,
    rrf_score * 0.5 + freshness * 0.5)
```

**Step 3** (future): Model calls in expressions:
```
xgboost("product_ranker.json") * 0.8 + freshness * 0.2
```

### 10.5 Judgment Collection API

Endpoints for collecting relevance judgments (human or LLM-generated):

```
POST /_query/judgments
{
  "query": "payment failed",
  "judgments": [
    {"topic": "logs", "partition": 2, "offset": 91827, "grade": 4, "judge": "human"},
    {"topic": "logs", "partition": 2, "offset": 91830, "grade": 1, "judge": "gpt-4"}
  ]
}

GET /_query/judgments?query=payment+failed
GET /_query/judgments/export?format=csv
```

Stored in `_chronik_judgments` internal topic. Exportable for offline model training.

### 10.6 Training Data Export

Export feature logs + judgments in LTR-ready formats:

```
GET /_query/training-data/export?format=libsvm
GET /_query/training-data/export?format=csv
GET /_query/training-data/export?format=xgboost
```

Workflow:
1. Users search with `/_query` (features logged automatically)
2. Users submit judgments via `/_query/judgments`
3. Users export training data
4. Users train XGBoost model offline
5. Users upload model file to `$CHRONIK_DATA_DIR/models/`
6. Users update profile to use `"provider": "xgboost"`

---

## Phase 11 — Policy Overlay + Governance

**Goal**: Enterprise-grade access control, business rules, compliance, and audit.

### 11.1 Policy Engine

```rust
pub trait PolicyEngine: Send + Sync {
    /// Filter candidates based on policies (access control, compliance).
    fn filter(&self, candidates: Vec<Candidate>, context: &PolicyContext) -> Vec<Candidate>;

    /// Apply business rules (boosting, burying, pinning).
    fn apply_rules(&self, candidates: Vec<Candidate>, rules: &[BusinessRule]) -> Vec<Candidate>;
}
```

### 11.2 Access Control

Per-topic, per-user access control applied BEFORE ranking:

```json
{
  "policies": [
    {
      "name": "engineering_access",
      "type": "access_control",
      "subjects": ["group:engineering"],
      "topics": ["logs", "events", "metrics"],
      "action": "allow"
    },
    {
      "name": "hr_restricted",
      "type": "access_control",
      "subjects": ["group:engineering"],
      "topics": ["hr", "payroll"],
      "action": "deny"
    }
  ]
}
```

### 11.3 Business Rules

Boosting, burying, pinning applied relative to ML ranking:

```json
{
  "rules": [
    {
      "name": "boost_featured",
      "type": "boost",
      "condition": {"topic": "products", "metadata.featured": true},
      "factor": 1.5,
      "stage": "before_ranking"
    },
    {
      "name": "pin_getting_started",
      "type": "pin",
      "condition": {"query_contains": "onboarding"},
      "pin_topic": "docs",
      "pin_offset": 42,
      "position": 0,
      "stage": "after_ranking"
    },
    {
      "name": "bury_stale",
      "type": "bury",
      "condition": {"age_hours_gt": 720},
      "factor": 0.3,
      "stage": "before_ranking"
    }
  ]
}
```

**Ordering** (following Algolia's model):
- Boosting/burying: applied BEFORE ranking (ML can adjust)
- Pinning/hiding: applied AFTER ranking (overrides ML)

### 11.4 Content Filtering

Compliance and safety filters:

```json
{
  "filters": [
    {
      "name": "pii_filter",
      "type": "regex_exclude",
      "pattern": "\\b\\d{3}-\\d{2}-\\d{4}\\b",
      "description": "Exclude results containing SSN patterns"
    },
    {
      "name": "compliance_region",
      "type": "metadata_filter",
      "field": "data_region",
      "allowed_values": ["us-east", "us-west"],
      "description": "Data sovereignty: only show US-region data"
    }
  ]
}
```

### 11.5 Audit Logging

Every query logs a comprehensive audit record to `_chronik_audit_log`:

```json
{
  "query_id": "q-abc123",
  "timestamp": "2026-02-22T10:15:00Z",
  "user": "alice@example.com",
  "groups": ["engineering"],
  "request_summary": "text='payment failed', topics=['logs','orders']",
  "results_shown": 50,
  "results_filtered_by_policy": 12,
  "policies_applied": ["engineering_access", "pii_filter"],
  "profile_used": "relevance",
  "latency_ms": 42
}
```

### 11.6 Query Understanding

LLM-powered query preprocessing:
- Query expansion ("DB" -> "database")
- Intent classification (navigational vs informational)
- Synonym generation
- Multi-language translation

---

## Phase Summary

| Phase | What Ships | ML Required? | Status |
|-------|-----------|-------------|--------|
| **9** | `/_query` endpoint, parallel retrieval, RRF, RuleRanker, JSON profiles, feature logging, merged+grouped results | No | **Complete** |
| **10** | Pluggable rerankers (Cohere, ONNX, LLM, GBDT), expression engine, judgment collection, training data export | Optional | Not started |
| **11** | Policy overlay, access control, business rules, content filtering, audit logging, query understanding | No | Not started |

**Key principle**: Each phase is independently valuable. Phase 9 alone gives users production-quality multi-backend search with transparent ranking. Phase 10 adds ML for users who want it. Phase 11 adds governance for enterprise users.

---

## Competitive Positioning After All Phases

```
                    Streaming    Search    Vector    SQL    Ranking    ML Models    Governance
Confluent/Kafka        X
Redpanda               X
Elasticsearch                      X        X              Rescore    LTR plugin
Vespa                              X        X       ~        X          X
Weaviate                           ~        X              Rerank
Pinecone                                    X              Rerank
Chronik (all phases)   X           X        X       X        X          X            X
```

**Chronik is the only platform that covers the entire horizontal.**

---

## Success Metrics

| Metric | Phase 9 Target | Phase 10 Target |
|--------|---------------|-----------------|
| `/_query` latency (p50) | < 50ms | < 100ms (with reranking) |
| `/_query` latency (p99) | < 200ms | < 500ms (with reranking) |
| Feature logging overhead | < 5% of query latency | < 5% |
| RRF merge overhead | < 2ms for 500 candidates | N/A |
| Profile hot reload | < 1s | < 1s |
| Backend timeout recovery | Partial results within timeout | Same |
