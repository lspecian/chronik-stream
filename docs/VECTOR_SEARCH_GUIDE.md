# Vector Search Guide

**Version**: v2.2.22+
**Feature Status**: Production Ready

Chronik Stream supports optional per-topic vector search using HNSW indexes and embeddings, enabling semantic search and similarity queries on your streaming data.

---

## Overview

Vector search provides:
- **Semantic Search**: Find messages by meaning, not just keywords
- **HNSW Indexes**: Fast approximate nearest neighbor search
- **Multiple Embedding Providers**: OpenAI, custom HTTP endpoints
- **Hybrid Search**: Combine vector and full-text search with score fusion
- **Filtering**: Search within partitions, offset ranges, or time windows

### Architecture

```
Producer ──▶ ProduceHandler ──▶ WAL (fsync) ──▶ Response to client
                                     │
                                     ▼ (background, async)
                                WalIndexer
                                     │
                 ┌───────────────────┼───────────────────┐
                 ▼                   ▼                   ▼
           [If searchable]   [If columnar.enabled]  [If vector.enabled]
           Tantivy Index     Parquet Segment        EmbeddingPipeline
                                                          │
                                                   ┌──────┴──────┐
                                                   ▼             ▼
                                              OpenAI API    HNSW Index
                                              (batch embed)  (per partition)
```

**Key Guarantees**:
- Embedding generation happens AFTER produce in the background
- Produce latency remains unchanged (~2-10ms)
- Embedding failures don't affect message durability
- Query availability: 30-90s after produce (embedding + indexing time)

---

## Quick Start

### 1. Create a Topic with Vector Search

```bash
# Using kafka-topics.sh
kafka-topics.sh --create --topic logs \
  --bootstrap-server localhost:9092 \
  --partitions 6 \
  --config vector.enabled=true \
  --config vector.embedding.provider=openai \
  --config vector.embedding.model=text-embedding-3-small \
  --config vector.field=value
```

### 2. Set Your API Key

```bash
# Set OpenAI API key in environment
export OPENAI_API_KEY="sk-..."

# Or configure via chronik config
export CHRONIK_EMBEDDING_API_KEY="sk-..."
```

### 3. Produce Messages

```python
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode('utf-8')
)

# Produce log messages
producer.send('logs', {'message': 'Database connection failed: timeout after 30s'})
producer.send('logs', {'message': 'User login successful for admin@example.com'})
producer.send('logs', {'message': 'Payment processed successfully for order ORD-12345'})
producer.flush()
```

### 4. Search Semantically

```bash
# Search by meaning (not keywords)
curl -X POST http://localhost:6092/_vector/logs/search \
  -H "Content-Type: application/json" \
  -d '{
    "query": "database connection issues",
    "k": 10
  }'
```

---

## Configuration Options

Configure vector search via topic configuration:

### Core Settings

| Config Key | Values | Default | Description |
|------------|--------|---------|-------------|
| `vector.enabled` | `true`, `false` | `false` | Enable vector search for this topic |
| `vector.field` | `value`, `key`, `$.json.path` | `value` | Field to embed |

### Embedding Provider

| Config Key | Values | Default | Description |
|------------|--------|---------|-------------|
| `vector.embedding.provider` | `openai`, `external` | `openai` | Embedding provider |
| `vector.embedding.model` | model name | `text-embedding-3-small` | Model to use |
| `vector.embedding.dimensions` | integer | model default | Vector dimensions |

### OpenAI Models

| Model | Dimensions | Description |
|-------|------------|-------------|
| `text-embedding-3-small` | 1536 | Fast, cost-effective (default) |
| `text-embedding-3-large` | 3072 | Higher quality |
| `text-embedding-ada-002` | 1536 | Legacy model |

### HNSW Index Settings

| Config Key | Values | Default | Description |
|------------|--------|---------|-------------|
| `vector.index.type` | `hnsw`, `flat` | `hnsw` | Index type |
| `vector.index.m` | integer | `16` | HNSW connections per layer |
| `vector.index.ef_construction` | integer | `200` | Build-time search width |
| `vector.index.ef_search` | integer | `50` | Query-time search width |
| `vector.index.metric` | `cosine`, `euclidean`, `dot` | `cosine` | Distance metric |

### Example: Full Configuration

```bash
kafka-topics.sh --create --topic support_tickets \
  --partitions 6 \
  --config vector.enabled=true \
  --config vector.embedding.provider=openai \
  --config vector.embedding.model=text-embedding-3-large \
  --config vector.embedding.dimensions=3072 \
  --config vector.field=value \
  --config vector.index.type=hnsw \
  --config vector.index.m=32 \
  --config vector.index.ef_construction=400 \
  --config vector.index.ef_search=100 \
  --config vector.index.metric=cosine
```

---

## Search API

### Semantic Search (by Text)

Embed your query and find similar messages:

```http
POST /_vector/{topic}/search
Content-Type: application/json

{
  "query": "database connection timeout errors",
  "k": 10,
  "filters": {
    "partitions": [0, 1, 2],
    "min_offset": 1000000,
    "max_offset": 2000000
  }
}
```

**Response:**
```json
{
  "results": [
    {
      "topic": "logs",
      "partition": 0,
      "offset": 1234567,
      "score": 0.92,
      "text_preview": "Database connection failed: timeout after 30s..."
    },
    {
      "topic": "logs",
      "partition": 1,
      "offset": 1234890,
      "score": 0.87,
      "text_preview": "MySQL connection pool exhausted, retrying..."
    }
  ],
  "query_embedding_time_ms": 45,
  "search_time_ms": 3
}
```

### Search by Vector

If you already have an embedding:

```http
POST /_vector/{topic}/search_by_vector
Content-Type: application/json

{
  "vector": [0.123, -0.456, 0.789, ...],
  "k": 10,
  "filters": {}
}
```

### Hybrid Search (Vector + Full-Text)

Combine semantic and keyword search using Reciprocal Rank Fusion (RRF):

```http
POST /_vector/{topic}/hybrid
Content-Type: application/json

{
  "query": "connection timeout error",
  "k": 10,
  "vector_weight": 0.7,
  "text_weight": 0.3
}
```

**Response:**
```json
{
  "results": [
    {
      "topic": "logs",
      "partition": 0,
      "offset": 1234567,
      "score": 0.85,
      "vector_score": 0.92,
      "text_score": 0.78,
      "text_preview": "Connection timeout error..."
    }
  ],
  "fusion_method": "rrf"
}
```

### Index Statistics

```http
GET /_vector/{topic}/stats
```

**Response:**
```json
{
  "topic": "logs",
  "total_vectors": 125000,
  "partitions": {
    "0": {"vectors": 42000, "last_offset": 1234567},
    "1": {"vectors": 41500, "last_offset": 1234890},
    "2": {"vectors": 41500, "last_offset": 1234456}
  },
  "index_type": "hnsw",
  "dimensions": 1536,
  "distance_metric": "cosine"
}
```

### List Vector-Enabled Topics

```http
GET /_vector/topics
```

**Response:**
```json
{
  "topics": ["logs", "support_tickets", "customer_feedback"]
}
```

---

## Search Filters

Filter search results by partition, offset, or time:

```json
{
  "query": "error",
  "k": 10,
  "filters": {
    "partitions": [0, 1],
    "min_offset": 1000000,
    "max_offset": 2000000,
    "min_timestamp": 1704067200000,
    "max_timestamp": 1704153600000
  }
}
```

| Filter | Type | Description |
|--------|------|-------------|
| `partitions` | `[i32]` | Search only these partitions |
| `min_offset` | `i64` | Minimum offset (inclusive) |
| `max_offset` | `i64` | Maximum offset (inclusive) |
| `min_timestamp` | `i64` | Minimum timestamp in ms (inclusive) |
| `max_timestamp` | `i64` | Maximum timestamp in ms (inclusive) |

---

## Embedding Providers

### OpenAI (Default)

```bash
# Required
export OPENAI_API_KEY="sk-..."

# Topic config
--config vector.embedding.provider=openai
--config vector.embedding.model=text-embedding-3-small
```

**Rate Limiting**: The OpenAI provider automatically handles 429 responses with exponential backoff.

### External HTTP Provider

For custom embedding services:

```bash
--config vector.embedding.provider=external
--config vector.embedding.endpoint=http://localhost:8080/embed
--config vector.embedding.dimensions=384
```

**Expected API**:
```http
POST /embed
Content-Type: application/json

{
  "texts": ["text 1", "text 2", ...]
}

Response:
{
  "embeddings": [[0.1, 0.2, ...], [0.3, 0.4, ...]]
}
```

---

## HNSW Index Tuning

### Parameters

| Parameter | Effect | Trade-off |
|-----------|--------|-----------|
| `m` | Connections per node | Higher = better recall, more memory |
| `ef_construction` | Build-time search width | Higher = better index, slower build |
| `ef_search` | Query-time search width | Higher = better recall, slower query |

### Recommended Configurations

**High Accuracy (Slow)**:
```bash
--config vector.index.m=48
--config vector.index.ef_construction=500
--config vector.index.ef_search=200
```

**Balanced (Default)**:
```bash
--config vector.index.m=16
--config vector.index.ef_construction=200
--config vector.index.ef_search=50
```

**Fast (Lower Accuracy)**:
```bash
--config vector.index.m=8
--config vector.index.ef_construction=100
--config vector.index.ef_search=20
```

### Distance Metrics

| Metric | Use Case |
|--------|----------|
| `cosine` | Text embeddings (default) |
| `euclidean` | L2 distance |
| `dot` | Inner product (normalized vectors) |

---

## Monitoring

### Prometheus Metrics

```
# Embedding pipeline
chronik_embedding_latency_ms{type="average"} 45
chronik_embedding_queue_depth 5
chronik_embedding_requests_total{result="success"} 12345
chronik_embedding_requests_total{result="error"} 12
chronik_embedding_tokens_total 5678900
chronik_embedding_messages_total 123456
chronik_embedding_vectors_total 123456

# Vector search
chronik_vector_search_total{topic="logs"} 5678
chronik_vector_search_latency_ms{type="average"} 3

# Index stats
chronik_vector_index_size{topic="logs"} 125000
```

### Health Check

```bash
curl http://localhost:6092/_vector/logs/stats
```

---

## Best Practices

### 1. Choose the Right Field to Embed

```bash
# Embed the message value (default)
--config vector.field=value

# Embed a specific JSON field
--config vector.field=$.message.content

# Embed the message key (if meaningful)
--config vector.field=key
```

### 2. Balance Quality vs Cost

| Model | Cost | Quality | Use Case |
|-------|------|---------|----------|
| `text-embedding-3-small` | Low | Good | Default, most use cases |
| `text-embedding-3-large` | Medium | Better | High-precision search |

### 3. Tune HNSW for Your Workload

- **More vectors** → Increase `m` and `ef_construction`
- **Need faster queries** → Decrease `ef_search`
- **Need better accuracy** → Increase `ef_search`

### 4. Use Filters Wisely

```json
{
  "query": "payment error",
  "k": 10,
  "filters": {
    "min_timestamp": 1704067200000
  }
}
```

Filtering by time/partition reduces search space significantly.

### 5. Monitor Embedding Pipeline

Watch these metrics:
- `chronik_embedding_queue_depth` - Should stay low
- `chronik_embedding_latency_ms` - Track API latency
- `chronik_embedding_errors_total` - Should be rare

---

## Troubleshooting

### Search Returns Empty Results

1. **Check topic has vector search enabled**:
   ```bash
   kafka-topics.sh --describe --topic logs | grep vector
   ```

2. **Check embedding pipeline is processing**:
   ```bash
   curl http://localhost:6092/_vector/logs/stats
   ```

3. **Check for embedding errors**:
   - Look for `chronik_embedding_errors_total` metric
   - Check logs for embedding API errors

4. **Wait for indexing**:
   - Vector search is available ~30-90s after produce
   - This includes embedding API latency

### Poor Search Quality

1. **Check the field being embedded**:
   - Ensure `vector.field` points to meaningful text
   - JSON path must extract actual content

2. **Try a better model**:
   ```bash
   --config vector.embedding.model=text-embedding-3-large
   ```

3. **Tune HNSW parameters**:
   - Increase `ef_search` for better recall
   - Increase `m` for better index quality

### High Embedding Costs

1. **Use smaller model**:
   ```bash
   --config vector.embedding.model=text-embedding-3-small
   ```

2. **Filter which messages get embedded**:
   - Only enable vector search on topics that need it
   - Use `vector.field` to embed specific content

3. **Monitor token usage**:
   - Track `chronik_embedding_tokens_total` metric

### Embedding Rate Limiting

The OpenAI provider handles rate limits automatically with exponential backoff. If you see many 429 errors:

1. **Check your OpenAI tier limits**
2. **Reduce batch size** (configured in WalIndexer)
3. **Consider an external provider** for high-volume topics

---

## API Reference

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/_vector/{topic}/search` | Semantic search by text |
| POST | `/_vector/{topic}/search_by_vector` | Search by vector |
| POST | `/_vector/{topic}/hybrid` | Hybrid vector + text search |
| GET | `/_vector/{topic}/stats` | Get index statistics |
| GET | `/_vector/topics` | List vector-enabled topics |

### Search Request Schema

```json
{
  "query": "string (required for search)",
  "vector": "[float] (required for search_by_vector)",
  "k": "integer (default: 10)",
  "filters": {
    "partitions": "[integer] (optional)",
    "min_offset": "integer (optional)",
    "max_offset": "integer (optional)",
    "min_timestamp": "integer ms (optional)",
    "max_timestamp": "integer ms (optional)"
  }
}
```

### Search Result Schema

```json
{
  "results": [
    {
      "topic": "string",
      "partition": "integer",
      "offset": "integer",
      "score": "float (0-1, higher = more similar)",
      "text_preview": "string (first 100 chars)"
    }
  ],
  "query_embedding_time_ms": "integer (for text queries)",
  "search_time_ms": "integer"
}
```

---

## Related Documentation

- [Columnar Storage Guide](COLUMNAR_STORAGE_GUIDE.md) - SQL queries on streaming data
- [Disaster Recovery](DISASTER_RECOVERY.md) - Backup and restore
- [Advanced Storage Roadmap](ADVANCED_STORAGE_ROADMAP.md) - Feature roadmap
