# Log Analysis Dashboard Example

End-to-end demo of Chronik's `/_query` orchestrator using a realistic microservices log analysis use case.

## What It Does

1. Starts a Chronik server with text search + columnar storage enabled
2. Produces **300 structured log messages** from 5 microservices via Kafka protocol
3. Waits for Tantivy (full-text) and Parquet (SQL) indexing
4. Runs **12 query scenarios** demonstrating every capability of the `/_query` endpoint
5. Cleans up on exit

## Prerequisites

```bash
# Build the server
cargo build --release --bin chronik-server

# Install dependencies
pip3 install kafka-python   # for Kafka protocol data production
sudo apt install jq         # for JSON processing
```

## Run

```bash
bash examples/log-analysis/demo.sh
```

The script is fully self-contained — it manages the server lifecycle automatically.

## Data Model

### app-logs (200 messages)

Structured application logs from 5 microservices:

| Field | Type | Example |
|-------|------|---------|
| `level` | string | `ERROR`, `WARN`, `INFO`, `DEBUG` |
| `service` | string | `payment-svc`, `auth-svc`, `order-svc`, `inventory-svc`, `notification-svc` |
| `msg` | string | `Payment gateway timeout after 30s — retrying` |
| `endpoint` | string | `/api/pay`, `/api/login`, `/api/orders` |
| `response_time_ms` | int | `1` - `5000` |
| `trace_id` | string | `trace-000042` |
| `timestamp` | int | Unix epoch ms |

### access-logs (100 messages)

HTTP access logs:

| Field | Type | Example |
|-------|------|---------|
| `method` | string | `GET`, `POST`, `PUT`, `DELETE` |
| `path` | string | `/api/orders`, `/api/users`, `/healthz` |
| `status` | int | `200`, `404`, `500`, `503` |
| `response_time_ms` | int | `1` - `5000` |
| `user_agent` | string | `Mozilla/5.0`, `curl/7.88` |
| `remote_ip` | string | `10.0.x.x` |
| `bytes_sent` | int | `100` - `50000` |

## Query Scenarios

### 1. Find payment errors (Full-text search)

```bash
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "app-logs", "modes": ["text"]}],
  "q": {"text": "payment gateway timeout"},
  "k": 5,
  "result_format": "merged"
}'
```

Searches all indexed app-logs for `payment gateway timeout` using BM25 scoring.

**Result**: 5 results, top score ~29.0

### 2. Boolean text search (OR queries)

```bash
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "app-logs", "modes": ["text"]}],
  "q": {"text": "connection refused OR pool exhausted"},
  "k": 10,
  "result_format": "merged"
}'
```

Finds logs matching either `connection refused` or `pool exhausted`.

**Result**: ~10 results from ~30 candidates

### 3. Record count (SQL)

```bash
curl -s localhost:6092/_sql -H 'Content-Type: application/json' -d '{
  "query": "SELECT COUNT(*) as total FROM app_logs"
}'
```

Direct SQL query via DataFusion. Topic names are sanitized (`app-logs` becomes `app_logs`).

**Result**: 200+ records in <1ms

### 4. Recent records by offset (SQL)

```bash
curl -s localhost:6092/_sql -H 'Content-Type: application/json' -d '{
  "query": "SELECT _offset, _partition, _topic, _timestamp FROM app_logs ORDER BY _offset DESC LIMIT 5"
}'
```

SQL ordering by Kafka offset to find the most recent entries.

**Result**: 5 rows with offset, partition, topic, timestamp columns

### 5. Access logs via /_query SQL mode

```bash
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "access-logs", "modes": ["sql"]}],
  "q": {"sql": "SELECT * FROM access_logs LIMIT 10"},
  "k": 10,
  "result_format": "merged"
}'
```

SQL query routed through the orchestrator (adds scoring and ranking on top of SQL results).

**Result**: 10 results with orchestrator metadata (query_id, stats, features)

### 6. Hybrid search (Text + Fetch with RRF)

```bash
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "app-logs", "modes": ["text", "fetch"]}],
  "q": {
    "text": "database connection",
    "fetch": {"offset": 0, "partition": 0, "max_bytes": 524288}
  },
  "k": 10,
  "result_format": "merged"
}'
```

Runs text search AND WAL fetch in parallel, then merges results using Reciprocal Rank Fusion (RRF). Documents found by both backends get boosted (`source_count: 2.0`).

**Result**: 10 results from ~30 candidates, top result features:
```json
{
  "rrf_score": 0.033,
  "source_count": 2.0,
  "text_rank": 1.0,
  "text_score": 11.48
}
```

### 7. Multi-topic search

```bash
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [
    {"topic": "app-logs", "modes": ["fetch"]},
    {"topic": "access-logs", "modes": ["fetch"]}
  ],
  "q": {"fetch": {"offset": 0, "partition": 0, "max_bytes": 1048576}},
  "k": 20,
  "result_format": "merged"
}'
```

Fan-out query across both topics in parallel. Results are merged and ranked together.

**Result**: Results spanning both `app-logs` and `access-logs`

### 8. Grouped results

Same query as #7 but with `"result_format": "grouped"`:

```json
{
  "grouped_results": {
    "access-logs": [/* results */],
    "app-logs": [/* results */]
  }
}
```

Results organized by topic instead of merged into a flat list.

### 9. Ranking profiles (Freshness vs Relevance)

```bash
# Freshness: prioritize recent logs
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "app-logs", "modes": ["text"]}],
  "q": {"text": "error"},
  "k": 5,
  "rank": {"profile": "freshness"},
  "result_format": "merged"
}'

# Relevance: prioritize text match quality
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "app-logs", "modes": ["text"]}],
  "q": {"text": "error"},
  "k": 5,
  "rank": {"profile": "relevance"},
  "result_format": "merged"
}'
```

Same query, different ranking. Freshness boosts recent results; relevance boosts high BM25 scores.

**Result**: Different ordering — freshness top score ~7.8, relevance top score ~44.0

### 10. SQL via orchestrator

SQL queries routed through `/_query` get the full orchestrator pipeline: scoring, ranking, and feature extraction.

**Result**: SQL rows enriched with `final_score`, `features`, `explanation`

### 11. Search with explanation

```bash
curl -s localhost:6092/_query -H 'Content-Type: application/json' -d '{
  "sources": [{"topic": "app-logs", "modes": ["text"]}],
  "q": {"text": "SSL certificate verification"},
  "k": 3,
  "result_format": "merged"
}'
```

Each result includes a human-readable explanation:

```
score=35.22 [rrf_score=0.017*100.00, source_count=1.000*10.00, text_rank=1.000*3.00, text_score=20.55*1.00]
```

Format: `feature_name=value*weight` — shows exactly how the final score was computed.

### 12. Capabilities introspection

```bash
# What can each topic do?
curl -s localhost:6092/_query/capabilities
# → [{"topic":"app-logs","text_search":true,"sql_query":true,"vector_search":true,"fetch":true}, ...]

# Available ranking profiles
curl -s localhost:6092/_query/profiles
# → [{"name":"default","weights":{...}}, {"name":"freshness","weights":{...}}, ...]
```

## API Reference

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/_query` | POST | Unified query orchestrator |
| `/_query/capabilities` | GET | Per-topic capability detection |
| `/_query/profiles` | GET | Available ranking profiles |
| `/_sql` | POST | Direct SQL queries (DataFusion) |
| `/_sql/tables` | GET | List queryable topics |

### `POST /_query` Request Format

```json
{
  "sources": [
    {
      "topic": "app-logs",
      "modes": ["text", "fetch", "sql", "vector"]
    }
  ],
  "q": {
    "text": "search query",
    "sql": "SELECT * FROM app_logs LIMIT 10",
    "fetch": {"offset": 0, "partition": 0, "max_bytes": 1048576}
  },
  "k": 10,
  "rank": {"profile": "default"},
  "result_format": "merged"
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `sources` | yes | Topics and query modes to use |
| `sources[].topic` | yes | Kafka topic name |
| `sources[].modes` | yes | Array of: `text`, `sql`, `fetch`, `vector` |
| `q` | yes | Query parameters (one per mode) |
| `q.text` | for text mode | Full-text search query |
| `q.sql` | for sql mode | SQL query string |
| `q.fetch` | for fetch mode | WAL fetch parameters |
| `k` | yes | Max results to return |
| `rank.profile` | no | Ranking profile: `default`, `freshness`, `relevance` |
| `result_format` | no | `merged` (flat list) or `grouped` (by topic) |

### `POST /_query` Response Format

```json
{
  "query_id": "uuid",
  "results": [
    {
      "topic": "app-logs",
      "partition": 0,
      "offset": 42,
      "source": "text",
      "raw_score": 29.15,
      "final_score": 35.22,
      "features": {
        "rrf_score": 0.017,
        "text_score": 20.55,
        "text_rank": 1.0,
        "source_count": 1.0
      },
      "explanation": "score=35.22 [rrf_score=0.017*100.00, ...]"
    }
  ],
  "stats": {
    "candidates": 30,
    "ranked": 10,
    "latency_ms": 1,
    "backends": [
      {"backend": "text", "topic": "app-logs", "candidates": 30, "latency_ms": 0, "timed_out": false}
    ]
  }
}
```

## Architecture

```
POST /_query
    |
    v
QueryHandler (parse request, validate)
    |
    v
TopicCapabilities (what can each topic do?)
    |
    v
QueryPlanner (build execution plan)
    |
    +-- TextNode(topic, query)      -> Tantivy SearchApi (BM25)
    +-- SqlNode(topic, query)       -> DataFusion (Parquet + hot buffer)
    +-- FetchNode(topic, offsets)    -> WalManager (WAL direct read)
    +-- VectorNode(topic, query)    -> HNSW (cosine similarity)
    |
    v  (parallel execution via tokio::spawn)
    |
CandidateCollector (normalize into Vec<Candidate>)
    |
    v
RRF Merger (per-topic: merge text+vector+sql+fetch ranks)
    |
    v
RuleRanker (apply profile weights + freshness/boost)
    |
    v
Response (ranked results + explanations + stats)
```

## Ranking Profiles

| Profile | Use Case | Key Weights |
|---------|----------|-------------|
| `default` | General purpose | Balanced: rrf=100, source_count=10, text/vector=3, freshness=5 |
| `freshness` | Real-time monitoring | Heavy freshness: freshness=50, rrf=50, source_count=5 |
| `relevance` | Search quality | Heavy text/vector: text=10, text_score=5, source_count=15 |

### Reciprocal Rank Fusion (RRF)

When multiple backends return results for the same topic, they are merged using RRF:

```
score(d) = SUM( 1 / (k + rank_i(d)) )
```

Where `k=60` (constant) and `rank_i` is the document's position in backend `i`'s result list. Documents found by multiple backends get higher scores because they accumulate terms from each backend.
