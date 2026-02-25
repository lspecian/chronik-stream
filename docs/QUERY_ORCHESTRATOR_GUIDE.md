# Query Orchestrator Guide

> **v2.3.0** — Multi-backend search with Reciprocal Rank Fusion

The Query Orchestrator (`/_query` endpoint) lets you search across **multiple topics** and **multiple backends** (full-text, vector, SQL, fetch) in a single request. Results are merged using Reciprocal Rank Fusion (RRF) and scored via configurable ranking profiles.

## Quick Start

```bash
# Start Chronik (single-node)
cargo run --bin chronik-server start

# Produce some data to a topic
kafka-console-producer.sh --topic logs --bootstrap-server localhost:9092

# Query it (full-text search)
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "logs", "modes": ["text"]}],
    "q": {"text": "connection timeout error"},
    "k": 10
  }'
```

## API Reference

### `POST /_query`

The unified query endpoint. All queries go through this single endpoint.

**Request body:**

```json
{
  "sources": [
    {"topic": "logs", "modes": ["text", "vector"]},
    {"topic": "orders", "modes": ["sql"]}
  ],
  "q": {
    "text": "payment failed",
    "semantic": "credit card processing error",
    "sql": "SELECT * FROM orders WHERE status = 'failed' LIMIT 100"
  },
  "filters": {
    "timestamp_gte": "2026-02-01T00:00:00Z",
    "timestamp_lte": "2026-02-25T23:59:59Z",
    "partitions": [0, 1, 2]
  },
  "k": 20,
  "rank": {"profile": "relevance"},
  "timeout_ms": 5000,
  "result_format": "merged"
}
```

**Fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `sources` | array | **required** | Topics and query modes to use |
| `sources[].topic` | string | **required** | Kafka topic name |
| `sources[].modes` | array | **required** | Query modes: `"text"`, `"vector"`, `"sql"`, `"fetch"` |
| `q.text` | string | null | Full-text query string (used by `text` mode) |
| `q.semantic` | string | null | Semantic query string (used by `vector` mode, embedded via OpenAI) |
| `q.sql` | string | null | SQL query (used by `sql` mode, executed via DataFusion) |
| `q.fetch` | object | null | Offset-based fetch parameters |
| `q.fetch.offset` | integer | **required** | Starting offset |
| `q.fetch.partition` | integer | 0 | Partition to fetch from |
| `q.fetch.max_bytes` | integer | 1048576 | Maximum bytes to fetch (1MB default) |
| `filters.timestamp_gte` | string | null | Minimum timestamp (ISO-8601 or epoch millis) |
| `filters.timestamp_lte` | string | null | Maximum timestamp (ISO-8601 or epoch millis) |
| `filters.partitions` | array | null | Limit to specific partitions |
| `k` | integer | 50 | Maximum results to return |
| `rank.profile` | string | `"default"` | Ranking profile name |
| `timeout_ms` | integer | 5000 | Query timeout in milliseconds |
| `result_format` | string | `"merged"` | `"merged"` (single list) or `"grouped"` (per-topic) |

**Response:**

```json
{
  "query_id": "550e8400-e29b-41d4-a716-446655440000",
  "results": [
    {
      "topic": "logs",
      "partition": 0,
      "offset": 42,
      "final_score": 3.96,
      "features": {
        "rrf_score": 0.033,
        "source_count": 2.0,
        "text_rank": 1.0,
        "vector_rank": 0.5,
        "freshness": 0.95
      },
      "explanation": "score=3.9600 [rrf_score=0.0330*100.00, source_count=2.0000*10.00, ...]",
      "text_preview": "{\"level\":\"ERROR\",\"msg\":\"connection timeout after 30s\"}"
    }
  ],
  "stats": {
    "candidates": 47,
    "ranked": 10,
    "latency_ms": 12,
    "backends": [
      {"backend": "text", "topic": "logs", "candidates": 30, "latency_ms": 4, "timed_out": false},
      {"backend": "vector", "topic": "logs", "candidates": 17, "latency_ms": 380, "timed_out": false}
    ]
  }
}
```

### `GET /_query/capabilities`

Returns which query modes are available per topic.

```bash
curl http://localhost:6092/_query/capabilities
```

```json
[
  {
    "topic": "logs",
    "text_search": true,
    "vector_search": true,
    "sql_query": true,
    "fetch": true
  },
  {
    "topic": "orders",
    "text_search": false,
    "vector_search": false,
    "sql_query": true,
    "fetch": true
  }
]
```

Capabilities are auto-detected from the server state:
- **text_search** — Tantivy index exists (created by WalIndexer or REST API)
- **vector_search** — HNSW index exists and embedding provider configured
- **sql_query** — Columnar storage (Parquet/HotBuffer) registered for topic
- **fetch** — Always true if topic exists (WAL)

### `GET /_query/profiles`

Returns available ranking profiles.

```bash
curl http://localhost:6092/_query/profiles
```

```json
[
  {"name": "default", "weights": {"rrf_score": 100, "freshness": 5, "source_count": 10, ...}},
  {"name": "freshness", "weights": {"rrf_score": 50, "freshness": 50, ...}},
  {"name": "relevance", "weights": {"rrf_score": 100, "freshness": 1, "source_count": 15, ...}}
]
```

## Query Modes

### Text Search (`"text"`)

Full-text search using Tantivy (BM25 scoring). Equivalent to `/_search` but integrated into the orchestrator.

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "logs", "modes": ["text"]}],
    "q": {"text": "OutOfMemoryError heap space"},
    "k": 10
  }'
```

**Requires:** Topic has been indexed by the WalIndexer (automatic, 30-60s after produce) or via the `/_search` REST API.

**Performance:** ~3-4ms p50 latency for 30K documents.

### Vector Search (`"vector"`)

Semantic search using HNSW index + OpenAI embeddings. The query text is embedded via the configured provider, then searched against pre-computed message embeddings.

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "logs", "modes": ["vector"]}],
    "q": {"semantic": "database connection pool exhausted"},
    "k": 10
  }'
```

**Requires:**
- Topic created with `vector.enabled=true`
- `OPENAI_API_KEY` environment variable set
- Messages have been embedded (automatic via WalIndexer)

**Performance:** ~370ms p50 latency (dominated by OpenAI embedding API call).

### SQL Query (`"sql"`)

Structured queries via DataFusion on columnar (Parquet/Arrow) storage.

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "orders", "modes": ["sql"]}],
    "q": {"sql": "SELECT * FROM orders WHERE amount > 100 ORDER BY timestamp DESC LIMIT 50"},
    "k": 50
  }'
```

**Requires:** Topic created with `columnar.enabled=true`.

**Performance:** ~6ms p50 latency for analytical queries on 30K records.

**Note:** SQL table names use sanitized topic names (`e2e-logs` becomes `e2e_logs`). The orchestrator handles this automatically.

### Fetch (`"fetch"`)

Direct offset-based retrieval from the WAL. No scoring or ranking — returns messages in offset order.

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "logs", "modes": ["fetch"]}],
    "q": {"fetch": {"offset": 1000, "partition": 0, "max_bytes": 1048576}},
    "k": 100
  }'
```

**Performance:** Sub-millisecond (direct WAL read).

## Hybrid Search

Combine multiple modes for the same topic. The orchestrator runs them in parallel, then merges with RRF.

```bash
# Text + Vector hybrid (best relevance quality)
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "logs", "modes": ["text", "vector"]}],
    "q": {
      "text": "connection refused",
      "semantic": "network connectivity failure"
    },
    "k": 10
  }'
```

**How RRF works:** Results from each backend are ranked independently. A document found at rank 2 in text and rank 5 in vector gets:

```
RRF score = 1/(60+2) + 1/(60+5) = 0.01613 + 0.01538 = 0.03151
```

Documents found by **multiple** backends always score higher than single-backend results. This naturally surfaces the most relevant matches without needing to normalize BM25 and cosine similarity onto a common scale.

## Multi-Topic Search

Query across multiple topics in a single request. Each topic can use different modes.

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [
      {"topic": "app-logs", "modes": ["text"]},
      {"topic": "system-logs", "modes": ["text"]},
      {"topic": "error-events", "modes": ["text", "vector"]}
    ],
    "q": {
      "text": "database timeout",
      "semantic": "slow query performance issue"
    },
    "k": 20
  }'
```

All topics are queried in parallel using `tokio::spawn`. The total latency is approximately the latency of the **slowest** backend, not the sum.

### Grouped Results

By default, results are merged into a single ranked list. Use `result_format: "grouped"` to keep results separated by topic:

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [
      {"topic": "app-logs", "modes": ["text"]},
      {"topic": "system-logs", "modes": ["text"]}
    ],
    "q": {"text": "error"},
    "k": 10,
    "result_format": "grouped"
  }'
```

Response:

```json
{
  "query_id": "...",
  "results": [],
  "grouped_results": {
    "app-logs": [
      {"topic": "app-logs", "offset": 42, "final_score": 1.67, ...},
      {"topic": "app-logs", "offset": 55, "final_score": 1.63, ...}
    ],
    "system-logs": [
      {"topic": "system-logs", "offset": 100, "final_score": 1.67, ...}
    ]
  },
  "stats": {...}
}
```

## Ranking Profiles

Ranking profiles control how candidates are scored after RRF fusion. Three built-in profiles are available, and you can create custom profiles.

### Built-in Profiles

#### `default`
Balanced scoring. Good for general-purpose search.

| Feature | Weight |
|---------|--------|
| `rrf_score` | 100 |
| `source_count` | 10 |
| `freshness` | 5 |
| `text_rank` | 3 |
| `vector_rank` | 3 |
| `sql_rank` | 3 |
| `text_score` | 1 |
| `vector_score` | 1 |

**Boost:** 1.2x multiplier when `source_count > 1` (multi-backend match)

#### `freshness`
Prioritizes recent messages. Use for real-time monitoring and log tailing.

| Feature | Weight |
|---------|--------|
| `rrf_score` | 50 |
| `freshness` | 50 |
| `source_count` | 5 |
| `text_rank` | 2 |
| `vector_rank` | 2 |
| `sql_rank` | 2 |

#### `relevance`
Prioritizes match quality. Use for precision-critical search.

| Feature | Weight |
|---------|--------|
| `rrf_score` | 100 |
| `source_count` | 15 |
| `text_rank` | 10 |
| `vector_rank` | 10 |
| `text_score` | 5 |
| `vector_score` | 5 |
| `freshness` | 1 |

**Boost:** 1.5x multiplier when `source_count > 1`

### Custom Profiles

Create a JSON file in `$CHRONIK_DATA_DIR/profiles/`:

```json
{
  "name": "high-freshness-boost",
  "weights": {
    "rrf_score": 80,
    "freshness": 40,
    "source_count": 10,
    "text_rank": 5,
    "vector_rank": 5
  },
  "boosts": [
    {
      "feature": "freshness",
      "op": "gt",
      "threshold": 0.5,
      "multiplier": 2.0
    }
  ]
}
```

Save as `$CHRONIK_DATA_DIR/profiles/high-freshness-boost.json`. Profiles are hot-reloaded every 30 seconds — no restart needed.

### Feature Reference

| Feature | Range | Description |
|---------|-------|-------------|
| `rrf_score` | 0-1 | Reciprocal Rank Fusion score (higher = found by more backends at higher ranks) |
| `source_count` | 1-4 | Number of backends that returned this candidate |
| `text_rank` | 0-1 | `1/(1+rank)` from text search results |
| `vector_rank` | 0-1 | `1/(1+rank)` from vector search results |
| `sql_rank` | 0-1 | `1/(1+rank)` from SQL query results |
| `text_score` | 0-∞ | Raw BM25 score from Tantivy |
| `vector_score` | 0-1 | Raw cosine similarity from HNSW |
| `freshness` | 0-1 | `1/(1+age_in_hours)` — decays as message ages |

### Boost Operators

| Operator | JSON | Description |
|----------|------|-------------|
| Greater than | `"gt"` | Feature value > threshold |
| Greater or equal | `"gte"` | Feature value >= threshold |
| Less than | `"lt"` | Feature value < threshold |
| Less or equal | `"lte"` | Feature value <= threshold |
| Equal | `"eq"` | Feature value == threshold |

## Timeout & Partial Results

The orchestrator uses per-query timeouts (default: 5s). If a backend doesn't respond in time:

1. The timed-out backend's candidates are dropped
2. Other backends' results are still returned
3. The `stats.backends` array shows `"timed_out": true` for the failed backend

```json
{
  "stats": {
    "backends": [
      {"backend": "text", "topic": "logs", "candidates": 30, "latency_ms": 4, "timed_out": false},
      {"backend": "vector", "topic": "logs", "candidates": 0, "latency_ms": 5001, "timed_out": true}
    ]
  }
}
```

This means vector search was slow (e.g., OpenAI API latency spike) but text results are still available. Use a longer timeout for vector-heavy queries:

```json
{"timeout_ms": 10000}
```

## Architecture

```
POST /_query
    │
    ▼
QueryHandler (parse request, validate, register SQL tables)
    │
    ▼
detect_topic_capabilities() (check Tantivy/HNSW/DataFusion/WAL per topic)
    │
    ▼
QueryPlanner (build execution plan: which backends, which topics)
    │
    ├─ TextNode(topic, query, k)      → Tantivy SearchApi
    ├─ VectorNode(topic, query, k)    → HNSW + OpenAI embedding
    ├─ SqlNode(topic, sql, limit)     → DataFusion ColumnarQueryEngine
    └─ FetchNode(topic, offset)       → WalManager
    │
    ▼  (parallel tokio::spawn, per-query timeout)
    │
CandidateSet (deduplicate by topic+partition+offset)
    │
    ▼
RRF Merger (score = Σ 1/(60 + rank_i) per candidate)
    │
    ▼
RuleRanker (weighted features + boost rules from profile)
    │
    ▼
Response (ranked results + explanations + per-backend stats)
```

Key design decisions:
- **RRF over score normalization:** BM25 scores, cosine similarities, and SQL ORDER BY positions are incommensurable. RRF uses only rank positions, making fusion robust and parameter-free.
- **Parallel execution:** All backends run as independent tokio tasks. Total latency = max(backend latencies), not sum.
- **Partial results:** Timed-out backends are dropped gracefully. The query always returns whatever results are available.
- **Backend adapter trait:** The orchestrator (`chronik-query`) is decoupled from backend implementations via `BackendAdapter`. The server (`chronik-server`) implements this trait against real backends.

## Prometheus Metrics

The orchestrator exposes metrics via the existing `chronik-monitoring` infrastructure:

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `chronik_query_requests_total` | counter | — | Total `/_query` requests |
| `chronik_query_latency_seconds` | histogram | — | End-to-end query latency |
| `chronik_query_candidates_total` | histogram | — | Candidates collected per query |
| `chronik_query_errors_total` | counter | `error_type` | Query errors (planning, execution) |

## Performance

Measured on a 3-node Hetzner k8s cluster (CCX33, 8 vCPU / 32GB RAM each) with 30K messages and 132K vector embeddings:

| Query Type | p50 | p95 | p99 | Notes |
|------------|-----|-----|-----|-------|
| Full-text (BM25) | **3.4ms** | 6.1ms | 8.2ms | Single topic, 30K docs |
| Vector (HNSW + OpenAI) | 372ms | 490ms | 524ms | Embedding API dominates |
| Hybrid (text + vector RRF) | 376ms | 510ms | 531ms | Parallel, max(text, vector) |
| SQL (DataFusion) | **5.9ms** | 9.8ms | 12.1ms | Analytical query, 30K rows |
| Multi-topic fan-out (3 topics) | 244ms | 560ms | 680ms | Text across 3 topics |

## Examples

### Log Analysis: Find errors across all log topics

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [
      {"topic": "app-logs", "modes": ["text"]},
      {"topic": "system-logs", "modes": ["text"]},
      {"topic": "nginx-logs", "modes": ["text"]}
    ],
    "q": {"text": "ERROR 5xx timeout"},
    "k": 25,
    "rank": {"profile": "freshness"}
  }'
```

### Semantic search with SQL fallback

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [
      {"topic": "support-tickets", "modes": ["vector", "sql"]}
    ],
    "q": {
      "semantic": "customer unable to complete checkout",
      "sql": "SELECT * FROM support_tickets WHERE priority = '\''high'\'' ORDER BY created_at DESC LIMIT 100"
    },
    "k": 20,
    "rank": {"profile": "relevance"}
  }'
```

### RAG chatbot: retrieve context for LLM

```bash
# Step 1: Query for relevant context
RESULTS=$(curl -s -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "knowledge-base", "modes": ["text", "vector"]}],
    "q": {
      "text": "how to reset password",
      "semantic": "user forgot their login credentials and needs to regain account access"
    },
    "k": 5,
    "rank": {"profile": "relevance"}
  }')

# Step 2: Extract text previews as context for your LLM
echo $RESULTS | jq -r '.results[].text_preview'
```

### Time-filtered query

```bash
curl -X POST http://localhost:6092/_query \
  -H "Content-Type: application/json" \
  -d '{
    "sources": [{"topic": "events", "modes": ["text"]}],
    "q": {"text": "deployment failed"},
    "filters": {
      "timestamp_gte": "2026-02-24T00:00:00Z",
      "timestamp_lte": "2026-02-25T23:59:59Z"
    },
    "k": 10
  }'
```

## Existing Endpoints (Still Available)

The Query Orchestrator adds `/_query` alongside the existing single-backend endpoints. All previous endpoints continue to work:

| Endpoint | Description | When to use |
|----------|-------------|-------------|
| `POST /_query` | Multi-backend orchestrated query **(new)** | Multi-topic, hybrid, ranked results |
| `POST /_sql` | Direct SQL query | Simple SQL, no ranking needed |
| `POST /_vector/{topic}/search` | Direct vector search | Single-topic vector, no fusion |
| `POST /_search` | Direct Elasticsearch-compatible search | ES client compatibility |
| Kafka Fetch API (port 9092) | Offset-based fetch | Real-time streaming consumption |
