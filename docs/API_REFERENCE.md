# Chronik Stream API Reference

**Version**: v2.2.22+
**Base URLs**:
- Kafka Protocol: `localhost:9092`
- Unified API: `localhost:6092`
- Admin API: `localhost:10001` (or `10000 + node_id` in cluster mode)

---

## Table of Contents

1. [Unified API (Port 6092)](#unified-api-port-6092)
   - [Health Check](#health-check)
   - [SQL Endpoints](#sql-endpoints)
   - [Vector Search Endpoints](#vector-search-endpoints)
   - [Search Endpoints (Elasticsearch-compatible)](#search-endpoints)
2. [Admin API](#admin-api)
   - [Cluster Management](#cluster-management)
   - [Schema Registry](#schema-registry)
3. [Kafka Protocol API](#kafka-protocol-api)
4. [Error Responses](#error-responses)

---

## Unified API (Port 6092)

The Unified API provides a single HTTP endpoint for all query and management operations.

### Health Check

#### GET /health

Returns the health status and enabled features.

**Response**:
```json
{
  "status": "ok",
  "version": "2.2.22",
  "sql_enabled": true,
  "vector_enabled": true,
  "search_enabled": true,
  "admin_enabled": true
}
```

---

### SQL Endpoints

#### POST /_sql

Execute a SQL query against columnar-enabled topics.

**Request**:
```json
{
  "query": "SELECT * FROM orders WHERE amount > 100 LIMIT 10"
}
```

**Response**:
```json
{
  "columns": ["offset", "partition", "timestamp", "key", "value"],
  "rows": [
    [12345, 0, 1704067200000, "ORD-001", "{\"order_id\":\"ORD-001\",\"amount\":150}"],
    [12346, 0, 1704067201000, "ORD-002", "{\"order_id\":\"ORD-002\",\"amount\":200}"]
  ],
  "row_count": 2,
  "execution_time_ms": 15
}
```

**Error Response**:
```json
{
  "error": "SQL syntax error: unexpected token at position 10",
  "error_code": "INVALID_QUERY"
}
```

---

#### POST /_sql/explain

Get the query execution plan without executing.

**Request**:
```json
{
  "query": "SELECT COUNT(*) FROM orders WHERE amount > 100"
}
```

**Response**:
```json
{
  "plan": "Projection: COUNT(*)\n  Aggregate: groupBy=[], aggr=[COUNT(*)]\n    Filter: amount > 100\n      TableScan: orders",
  "estimated_rows": 1000
}
```

---

#### GET /_sql/tables

List all queryable topics (topics with `columnar.enabled=true`).

**Response**:
```json
{
  "tables": ["orders", "logs", "events", "metrics"]
}
```

---

#### GET /_sql/describe/{table}

Get the schema for a specific table.

**Path Parameters**:
- `table` (string, required): Topic name

**Response**:
```json
{
  "table": "orders",
  "schema": [
    {"name": "offset", "type": "Int64", "nullable": false},
    {"name": "partition", "type": "Int32", "nullable": false},
    {"name": "timestamp", "type": "Timestamp(Millisecond, UTC)", "nullable": false},
    {"name": "key", "type": "Binary", "nullable": true},
    {"name": "value", "type": "Binary", "nullable": false},
    {"name": "headers", "type": "Map(Utf8, Binary)", "nullable": true}
  ]
}
```

---

### Vector Search Endpoints

#### POST /_vector/{topic}/search

Semantic search by text query.

**Path Parameters**:
- `topic` (string, required): Topic name with vector search enabled

**Request**:
```json
{
  "query": "database connection timeout errors",
  "k": 10,
  "filters": {
    "partitions": [0, 1, 2],
    "min_offset": 1000000,
    "max_offset": 2000000,
    "min_timestamp": 1704067200000,
    "max_timestamp": 1704153600000
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `query` | string | Yes | Natural language search query |
| `k` | integer | No | Number of results to return (default: 10, max: 1000) |
| `filters.partitions` | [integer] | No | Filter to specific partitions |
| `filters.min_offset` | integer | No | Minimum offset (inclusive) |
| `filters.max_offset` | integer | No | Maximum offset (inclusive) |
| `filters.min_timestamp` | integer | No | Minimum timestamp in ms (inclusive) |
| `filters.max_timestamp` | integer | No | Maximum timestamp in ms (inclusive) |

**Response**:
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
  "search_time_ms": 3,
  "total_results": 2
}
```

---

#### POST /_vector/{topic}/search_by_vector

Search using a pre-computed embedding vector.

**Path Parameters**:
- `topic` (string, required): Topic name with vector search enabled

**Request**:
```json
{
  "vector": [0.123, -0.456, 0.789, ...],
  "k": 10,
  "filters": {}
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `vector` | [float] | Yes | Embedding vector (must match topic's dimension) |
| `k` | integer | No | Number of results (default: 10) |
| `filters` | object | No | Same as search endpoint |

**Response**: Same format as `/search`

---

#### POST /_vector/{topic}/hybrid

Hybrid search combining vector similarity and full-text matching.

**Path Parameters**:
- `topic` (string, required): Topic name with both vector and text search enabled

**Request**:
```json
{
  "query": "connection timeout error",
  "k": 10,
  "vector_weight": 0.7,
  "text_weight": 0.3,
  "filters": {}
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `query` | string | Yes | Search query (used for both vector and text) |
| `k` | integer | No | Number of results (default: 10) |
| `vector_weight` | float | No | Weight for vector results (default: 0.5) |
| `text_weight` | float | No | Weight for text results (default: 0.5) |
| `filters` | object | No | Same as search endpoint |

**Response**:
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
      "text_preview": "Connection timeout error after 30 seconds..."
    }
  ],
  "fusion_method": "rrf",
  "query_embedding_time_ms": 45,
  "search_time_ms": 8
}
```

---

#### GET /_vector/{topic}/stats

Get vector index statistics for a topic.

**Path Parameters**:
- `topic` (string, required): Topic name

**Response**:
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
  "distance_metric": "cosine",
  "embedding_provider": "openai",
  "embedding_model": "text-embedding-3-small"
}
```

---

#### GET /_vector/topics

List all topics with vector search enabled.

**Response**:
```json
{
  "topics": [
    {
      "name": "logs",
      "vectors": 125000,
      "dimensions": 1536,
      "provider": "openai"
    },
    {
      "name": "support_tickets",
      "vectors": 50000,
      "dimensions": 1536,
      "provider": "openai"
    }
  ]
}
```

---

### Search Endpoints

Elasticsearch-compatible search endpoints for full-text search via Tantivy.

#### POST /_search

Global search across all searchable topics.

**Request**:
```json
{
  "query": {
    "match": {
      "value": "error connection timeout"
    }
  },
  "size": 10,
  "from": 0
}
```

**Response**:
```json
{
  "hits": {
    "total": {"value": 42, "relation": "eq"},
    "hits": [
      {
        "_index": "logs",
        "_id": "logs-0-1234567",
        "_score": 1.5,
        "_source": {
          "offset": 1234567,
          "partition": 0,
          "timestamp": 1704067200000,
          "value": "Connection timeout error..."
        }
      }
    ]
  },
  "took": 15
}
```

---

#### POST /{index}/_search

Search within a specific topic (index).

**Path Parameters**:
- `index` (string, required): Topic name

**Request**: Same as `/_search`

**Response**: Same as `/_search`

---

#### GET /{index}/_doc/{id}

Get a specific document by ID.

**Path Parameters**:
- `index` (string, required): Topic name
- `id` (string, required): Document ID (format: `{topic}-{partition}-{offset}`)

**Response**:
```json
{
  "_index": "logs",
  "_id": "logs-0-1234567",
  "found": true,
  "_source": {
    "offset": 1234567,
    "partition": 0,
    "timestamp": 1704067200000,
    "key": "key-value",
    "value": "message content..."
  }
}
```

---

## Admin API

The Admin API runs on port `10000 + node_id` (e.g., 10001 for node 1).

**Authentication**: All endpoints except `/admin/health` require the `X-API-Key` header.

```bash
curl -H "X-API-Key: your-api-key" http://localhost:10001/admin/status
```

### Cluster Management

#### GET /admin/health

Health check (no authentication required).

**Response**:
```json
{
  "status": "healthy",
  "node_id": 1,
  "is_leader": true
}
```

---

#### GET /admin/status

Get detailed cluster status.

**Headers**:
- `X-API-Key` (required): Admin API key

**Response**:
```json
{
  "cluster": {
    "nodes": 3,
    "leader": 1,
    "term": 42
  },
  "nodes": [
    {
      "id": 1,
      "kafka_addr": "localhost:9092",
      "wal_addr": "localhost:9291",
      "raft_addr": "localhost:5001",
      "is_leader": true,
      "state": "Leader"
    },
    {
      "id": 2,
      "kafka_addr": "localhost:9093",
      "wal_addr": "localhost:9292",
      "raft_addr": "localhost:5002",
      "is_leader": false,
      "state": "Follower"
    }
  ],
  "partitions": [
    {
      "topic": "orders",
      "partition": 0,
      "leader": 1,
      "replicas": [1, 2, 3],
      "isr": [1, 2, 3]
    }
  ]
}
```

---

#### POST /admin/add-node

Add a new node to the cluster.

**Headers**:
- `X-API-Key` (required): Admin API key

**Request**:
```json
{
  "node_id": 4,
  "kafka_addr": "node4:9092",
  "wal_addr": "node4:9291",
  "raft_addr": "node4:5001"
}
```

**Response**:
```json
{
  "success": true,
  "message": "Node 4 added to cluster",
  "new_cluster_size": 4
}
```

---

#### POST /admin/remove-node

Remove a node from the cluster.

**Headers**:
- `X-API-Key` (required): Admin API key

**Request**:
```json
{
  "node_id": 4,
  "force": false
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `node_id` | integer | Yes | ID of node to remove |
| `force` | boolean | No | Skip partition reassignment (for dead nodes) |

**Response**:
```json
{
  "success": true,
  "message": "Node 4 removed from cluster",
  "partitions_reassigned": 12,
  "new_cluster_size": 3
}
```

---

### Schema Registry

Confluent Schema Registry compatible API.

#### GET /subjects

List all registered subjects.

**Response**:
```json
["user-value", "order-value", "event-key"]
```

---

#### GET /subjects/{subject}/versions

List all versions for a subject.

**Path Parameters**:
- `subject` (string, required): Subject name

**Response**:
```json
[1, 2, 3]
```

---

#### GET /subjects/{subject}/versions/{version}

Get a specific schema version.

**Path Parameters**:
- `subject` (string, required): Subject name
- `version` (string, required): Version number or "latest"

**Response**:
```json
{
  "subject": "user-value",
  "version": 2,
  "id": 5,
  "schemaType": "AVRO",
  "schema": "{\"type\":\"record\",\"name\":\"User\",\"fields\":[{\"name\":\"id\",\"type\":\"long\"},{\"name\":\"name\",\"type\":\"string\"}]}"
}
```

---

#### POST /subjects/{subject}/versions

Register a new schema version.

**Path Parameters**:
- `subject` (string, required): Subject name

**Request**:
```json
{
  "schemaType": "AVRO",
  "schema": "{\"type\":\"record\",\"name\":\"User\",\"fields\":[{\"name\":\"id\",\"type\":\"long\"},{\"name\":\"name\",\"type\":\"string\"},{\"name\":\"email\",\"type\":\"string\"}]}"
}
```

**Response**:
```json
{
  "id": 6
}
```

---

#### GET /schemas/ids/{id}

Get a schema by its global ID.

**Path Parameters**:
- `id` (integer, required): Schema ID

**Response**:
```json
{
  "schema": "{\"type\":\"record\",\"name\":\"User\",...}"
}
```

---

#### GET /config

Get global compatibility level.

**Response**:
```json
{
  "compatibilityLevel": "BACKWARD"
}
```

---

#### PUT /config

Set global compatibility level.

**Request**:
```json
{
  "compatibility": "FULL"
}
```

**Response**:
```json
{
  "compatibility": "FULL"
}
```

---

## Kafka Protocol API

Chronik implements the Kafka wire protocol on port 9092. Use any Kafka client.

### Supported APIs

| API | Versions | Description |
|-----|----------|-------------|
| Produce | 0-9 | Publish messages |
| Fetch | 0-13 | Consume messages |
| ListOffsets | 0-7 | Get partition offsets |
| Metadata | 0-12 | Topic/broker metadata |
| ApiVersions | 0-3 | Supported API versions |
| CreateTopics | 0-7 | Create topics |
| DeleteTopics | 0-6 | Delete topics |
| FindCoordinator | 0-4 | Consumer group coordinator |
| JoinGroup | 0-9 | Join consumer group |
| SyncGroup | 0-5 | Sync group assignments |
| Heartbeat | 0-4 | Consumer heartbeat |
| LeaveGroup | 0-5 | Leave consumer group |
| OffsetCommit | 0-8 | Commit offsets |
| OffsetFetch | 0-8 | Fetch committed offsets |
| DescribeConfigs | 0-4 | Describe configurations |
| AlterConfigs | 0-2 | Alter configurations |
| DescribeGroups | 0-5 | Describe consumer groups |
| ListGroups | 0-4 | List consumer groups |
| DeleteGroups | 0-2 | Delete consumer groups |

---

## Error Responses

All API endpoints return consistent error responses.

### Error Format

```json
{
  "error": "Human-readable error message",
  "error_code": "ERROR_CODE",
  "details": {}
}
```

### Common Error Codes

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `INVALID_QUERY` | 400 | SQL syntax error or invalid query |
| `TOPIC_NOT_FOUND` | 404 | Topic does not exist |
| `FEATURE_DISABLED` | 400 | Feature not enabled for topic |
| `UNAUTHORIZED` | 401 | Missing or invalid API key |
| `RATE_LIMITED` | 429 | Too many requests |
| `INTERNAL_ERROR` | 500 | Server error |
| `EMBEDDING_ERROR` | 502 | Embedding API error |
| `TIMEOUT` | 504 | Query timeout |

### Example Error Responses

**Topic not found**:
```json
{
  "error": "Topic 'nonexistent' not found",
  "error_code": "TOPIC_NOT_FOUND"
}
```

**Feature not enabled**:
```json
{
  "error": "Vector search not enabled for topic 'orders'. Set vector.enabled=true",
  "error_code": "FEATURE_DISABLED"
}
```

**Unauthorized**:
```json
{
  "error": "Invalid or missing API key",
  "error_code": "UNAUTHORIZED"
}
```

---

## Rate Limits

Default rate limits (configurable):

| Endpoint Category | Limit | Window |
|-------------------|-------|--------|
| SQL Queries | 100/min | Per IP |
| Vector Search | 60/min | Per IP |
| Admin API | 30/min | Per API key |
| Schema Registry | 100/min | Per IP |

Rate limit headers are included in responses:
```
X-RateLimit-Limit: 100
X-RateLimit-Remaining: 95
X-RateLimit-Reset: 1704067260
```

---

## Related Documentation

- [Columnar Storage Guide](COLUMNAR_STORAGE_GUIDE.md)
- [Vector Search Guide](VECTOR_SEARCH_GUIDE.md)
- [Admin API Security](ADMIN_API_SECURITY.md)
- [Schema Registry](SCHEMA_REGISTRY.md)
