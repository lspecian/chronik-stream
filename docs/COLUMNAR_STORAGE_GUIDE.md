# Columnar Storage Guide

**Version**: v2.2.22+
**Feature Status**: Production Ready

Chronik Stream supports optional per-topic columnar storage using Apache Arrow and Parquet, enabling SQL queries via DataFusion and high-performance analytics on your streaming data.

---

## Overview

Columnar storage provides:
- **SQL Query Support**: Full SQL via Apache DataFusion
- **Efficient Analytics**: Columnar format optimized for aggregations
- **Predicate Pushdown**: Filter at storage level for fast queries
- **Compression**: Industry-standard codecs (zstd, snappy, lz4)
- **Time Partitioning**: Hourly/daily partitions for efficient pruning

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
           Tantivy Index     Parquet Segment        HNSW + Embeddings
                                     │
                                     ▼
                              Object Store (S3/GCS/Azure/Local)
```

**Key Guarantee**: Columnar processing happens AFTER produce in the background WalIndexer pipeline. Produce latency remains unchanged (~2-10ms).

---

## Quick Start

### 1. Create a Topic with Columnar Storage

```bash
# Using kafka-topics.sh
kafka-topics.sh --create --topic orders \
  --bootstrap-server localhost:9092 \
  --partitions 6 \
  --config columnar.enabled=true \
  --config columnar.format=parquet \
  --config columnar.compression=zstd
```

### 2. Produce Messages

```python
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode('utf-8')
)

# Produce order events
producer.send('orders', {
    'order_id': 'ORD-12345',
    'customer_id': 'CUST-001',
    'amount': 99.99,
    'items': ['SKU-A', 'SKU-B']
})
producer.flush()
```

### 3. Query with SQL

```bash
# Query via the Unified API (port 6092)
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{
    "query": "SELECT * FROM orders WHERE amount > 50 ORDER BY timestamp DESC LIMIT 10"
  }'
```

---

## Configuration Options

Configure columnar storage via topic configuration:

| Config Key | Values | Default | Description |
|------------|--------|---------|-------------|
| `columnar.enabled` | `true`, `false` | `false` | Enable columnar storage for this topic |
| `columnar.format` | `parquet`, `arrow` | `parquet` | Storage format |
| `columnar.compression` | `zstd`, `snappy`, `lz4`, `none` | `zstd` | Compression codec |
| `columnar.row_group_size` | integer | `100000` | Rows per Parquet row group |
| `columnar.bloom_filter` | `true`, `false` | `true` | Enable bloom filters |
| `columnar.bloom_filter.columns` | comma-separated | `offset,timestamp,key` | Columns with bloom filters |
| `columnar.partitioning` | `none`, `hourly`, `daily` | `daily` | Time-based partitioning |

### Example: Full Configuration

```bash
kafka-topics.sh --create --topic logs \
  --partitions 12 \
  --config columnar.enabled=true \
  --config columnar.format=parquet \
  --config columnar.compression=zstd \
  --config columnar.row_group_size=50000 \
  --config columnar.bloom_filter=true \
  --config columnar.bloom_filter.columns=offset,timestamp,key,partition \
  --config columnar.partitioning=hourly
```

---

## SQL Query API

### Execute Query

```http
POST /_sql
Content-Type: application/json

{
  "query": "SELECT * FROM orders WHERE customer_id = 'CUST-001' LIMIT 100"
}
```

**Response:**
```json
{
  "columns": ["offset", "partition", "timestamp", "key", "value"],
  "rows": [
    [12345, 0, 1704067200000, "ORD-12345", "{\"order_id\":\"ORD-12345\",...}"],
    ...
  ],
  "row_count": 42,
  "execution_time_ms": 15
}
```

### Explain Query Plan

```http
POST /_sql/explain
Content-Type: application/json

{
  "query": "SELECT COUNT(*) FROM orders WHERE amount > 100"
}
```

### List Tables (Topics)

```http
GET /_sql/tables
```

**Response:**
```json
{
  "tables": ["orders", "logs", "events"]
}
```

### Describe Schema

```http
GET /_sql/describe/orders
```

**Response:**
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

## Schema

Each Kafka message is converted to a row with these columns:

| Column | Arrow Type | Nullable | Description |
|--------|------------|----------|-------------|
| `offset` | Int64 | No | Kafka message offset |
| `partition` | Int32 | No | Kafka partition number |
| `timestamp` | Timestamp(ms, UTC) | No | Message timestamp |
| `key` | Binary | Yes | Message key |
| `value` | Binary | No | Message value |
| `headers` | Map<Utf8, Binary> | Yes | Kafka headers |
| `_embedding` | FixedSizeList<Float32> | Yes | Vector embedding (if vector.enabled) |

### Querying JSON Values

If your messages are JSON, use DataFusion's JSON functions:

```sql
-- Extract JSON field
SELECT json_extract_scalar(value, '$.order_id') as order_id FROM orders

-- Filter by JSON field
SELECT * FROM orders
WHERE CAST(json_extract_scalar(value, '$.amount') AS DOUBLE) > 100

-- Aggregate
SELECT
  json_extract_scalar(value, '$.customer_id') as customer,
  COUNT(*) as order_count,
  SUM(CAST(json_extract_scalar(value, '$.amount') AS DOUBLE)) as total
FROM orders
GROUP BY json_extract_scalar(value, '$.customer_id')
```

---

## File Layout

Parquet files are organized by topic, partition, and time:

```
{data_dir}/columnar/
├── orders/
│   ├── partition=0/
│   │   ├── 2024-01-01/
│   │   │   ├── segment_0000000000.parquet
│   │   │   └── segment_0000001000.parquet
│   │   └── 2024-01-02/
│   │       └── segment_0000002000.parquet
│   └── partition=1/
│       └── ...
└── logs/
    └── ...
```

### Partitioning Strategies

| Strategy | Path Pattern | Use Case |
|----------|--------------|----------|
| `none` | `{topic}/partition={p}/segment_{offset}.parquet` | Small topics |
| `hourly` | `{topic}/partition={p}/{YYYY-MM-DD-HH}/segment_{offset}.parquet` | High-volume, recent queries |
| `daily` | `{topic}/partition={p}/{YYYY-MM-DD}/segment_{offset}.parquet` | Default, balanced |

---

## Performance Tuning

### Compression Selection

| Codec | Ratio | Speed | Use Case |
|-------|-------|-------|----------|
| `zstd` | Best (~4-5x) | Medium | Default, balanced |
| `snappy` | Good (~2-3x) | Fastest | Low-latency queries |
| `lz4` | Good (~2-3x) | Fast | Good balance |
| `none` | 1x | N/A | Maximum query speed |

### Row Group Size

- **Larger** (100K-1M): Better compression, slower point queries
- **Smaller** (10K-50K): Faster point queries, less compression
- **Default** (100K): Good balance for mixed workloads

### Bloom Filters

Enable bloom filters for columns you frequently filter on:

```bash
--config columnar.bloom_filter=true
--config columnar.bloom_filter.columns=offset,timestamp,key,customer_id
```

---

## Monitoring

### Prometheus Metrics

```
# Parquet segment creation
chronik_columnar_segments_created_total{topic="orders"} 42

# Query execution
chronik_sql_queries_total{result="success"} 1234
chronik_sql_queries_total{result="error"} 5
chronik_sql_query_latency_ms{type="average"} 15

# Storage usage
chronik_columnar_storage_bytes{topic="orders"} 1073741824
```

### Query Performance

Use the explain endpoint to understand query plans:

```bash
curl -X POST http://localhost:6092/_sql/explain \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT * FROM orders WHERE offset > 1000000"}'
```

Look for:
- **Predicate pushdown**: Filters should be pushed to Parquet scan
- **Projection pushdown**: Only required columns should be read
- **Partition pruning**: Time filters should skip irrelevant partitions

---

## Best Practices

### 1. Choose Appropriate Topics

Enable columnar storage for topics that benefit from SQL analytics:
- Order/transaction streams (aggregations, reporting)
- Log analytics (filtering, searching)
- Metrics/events (time-series analysis)

Don't enable for:
- High-throughput, simple consume patterns
- Topics where data is never queried via SQL

### 2. Use Time-Based Partitioning

```bash
--config columnar.partitioning=daily
```

Benefits:
- Efficient time-range queries
- Easy data lifecycle management
- Better cache locality

### 3. Configure Bloom Filters Wisely

Only add bloom filters for columns you filter on:
```bash
--config columnar.bloom_filter.columns=customer_id,order_id
```

### 4. Monitor Indexing Lag

Check the WalIndexer metrics to ensure background processing keeps up:
```
chronik_wal_indexer_lag_seconds
```

If lag grows consistently, consider:
- Increasing indexer batch size
- Adding more partitions
- Using faster compression

---

## Troubleshooting

### Queries Return Empty Results

1. **Check topic has columnar enabled**:
   ```bash
   kafka-topics.sh --describe --topic orders | grep columnar
   ```

2. **Check indexer is running**:
   ```bash
   curl http://localhost:6092/admin/status
   ```

3. **Check for indexing lag**:
   - Parquet files are created ~30-60s after produce
   - Check `chronik_wal_indexer_lag_seconds` metric

### Query Performance Issues

1. **Check query plan**:
   ```bash
   POST /_sql/explain
   ```

2. **Add appropriate indexes**:
   - Enable bloom filters for filtered columns
   - Use time partitioning for time-range queries

3. **Tune row group size**:
   - Smaller for point queries
   - Larger for full-scan analytics

### High Storage Usage

1. **Use compression**:
   ```bash
   --config columnar.compression=zstd
   ```

2. **Reduce retention**:
   - Old Parquet files follow WAL retention policies

---

## API Reference

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/_sql` | Execute SQL query |
| POST | `/_sql/explain` | Explain query plan |
| GET | `/_sql/tables` | List queryable topics |
| GET | `/_sql/describe/:table` | Describe table schema |

### SQL Functions

DataFusion provides extensive SQL support:
- Aggregates: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `STDDEV`, etc.
- String: `CONCAT`, `LENGTH`, `LOWER`, `UPPER`, `TRIM`, etc.
- JSON: `json_extract_scalar`, `json_extract`
- Time: `DATE_TRUNC`, `EXTRACT`, `TO_TIMESTAMP`, etc.

See [DataFusion SQL Reference](https://datafusion.apache.org/user-guide/sql/index.html) for full documentation.

---

## Related Documentation

- [Vector Search Guide](VECTOR_SEARCH_GUIDE.md) - Semantic search with embeddings
- [Disaster Recovery](DISASTER_RECOVERY.md) - Backup and restore
- [Advanced Storage Roadmap](ADVANCED_STORAGE_ROADMAP.md) - Feature roadmap
