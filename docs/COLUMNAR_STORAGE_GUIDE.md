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
| `columnar.projection` | `auto`, `otlp-logs`, `none` | `auto` | Which typed columns are projected out of JSON values (see [Projected Columns](#projected-columns)) |
| `columnar.columns` | `name:type:/pointer;...` | none | Declared projected columns |
| `columnar.projection.attributes` | `key;key` | none | `otlp-logs`: attributes projected as `attr_<key>` |

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
  "execution_time_ms": 15,
  "truncated": false
}
```

**Row limits.** A query's own `LIMIT n` is honoured (up to 100,000 rows). Queries
without one return at most 1,000 rows, and set `"truncated": true` when rows were
dropped. Pass `"limit": <n>` in the request body to override both. Aggregates
(`COUNT`, `SUM`, `GROUP BY`) are never limited by this — the cap applies to rows
returned, not rows scanned: aggregation always scans the whole table (hot WAL
records plus every Parquet segment).

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

**Response** (abridged):
```json
{
  "table": "orders",
  "columns": [
    {"name": "_topic", "data_type": "Utf8", "nullable": false},
    {"name": "_partition", "data_type": "Int32", "nullable": false},
    {"name": "_offset", "data_type": "Int64", "nullable": false},
    {"name": "_timestamp", "data_type": "Timestamp(Millisecond, Some(\"UTC\"))", "nullable": false},
    {"name": "_timestamp_type", "data_type": "Int8", "nullable": false},
    {"name": "_key", "data_type": "Binary", "nullable": true},
    {"name": "_value", "data_type": "Binary", "nullable": false},
    {"name": "order_id", "data_type": "Utf8", "nullable": true},
    {"name": "amount", "data_type": "Float64", "nullable": true}
  ]
}
```

---

## Schema

Every row carries the Kafka envelope:

| Column | Arrow Type | Nullable | Description |
|--------|------------|----------|-------------|
| `_topic` | Utf8 | No | Topic name |
| `_partition` | Int32 | No | Partition |
| `_offset` | Int64 | No | Offset |
| `_timestamp` | Timestamp(ms, UTC) | No | Record timestamp |
| `_timestamp_type` | Int8 | No | 0 = CreateTime, 1 = LogAppendTime |
| `_key` | Binary | Yes | Record key |
| `_value` | Binary | No | Record value, exactly as produced |
| `_headers` | Map<Utf8, Binary> | Yes | Headers (`{topic}_cold` only) |

plus the topic's **projected columns**, described next. `{topic}` is the
unified table (recent records from the hot buffer plus Parquet);
`{topic}_hot` and `{topic}_cold` are its two halves.

### Projected Columns

A JSON value's fields can be stored as typed columns of their own, written into
the Parquet segments beside `_value`. A filter or aggregate on such a column
reads that one narrow column (with Parquet min/max pruning) instead of decoding
every `_value`:

```sql
SELECT count(*) FROM default_logs WHERE severity = 'ERROR';
SELECT service_name, count(*) FROM default_logs GROUP BY service_name;
```

Which columns a topic projects is set by topic configs:

| Config | Values | Default |
|--------|--------|---------|
| `columnar.projection` | `auto`, `otlp-logs`, `none` | `auto` |
| `columnar.columns` | declared columns, `;`-separated (below) | none |
| `columnar.projection.attributes` | `otlp-logs` only: attribute keys, `;`-separated | none |

- **`auto`** infers the top-level fields of the values (up to 64) and projects
  each as its JSON type; nested objects and arrays become their JSON text. The
  list is kept in the topic config as `columnar.json_schema` and only grows: a
  field first seen later is added; a field keeps the type it was first given.
- **`otlp-logs`** projects one OpenTelemetry log record per message:
  `timestamp`, `observed_timestamp` (timestamp), `severity`, `severity_text`,
  `severity_number` (int64), `body`, `service_name` (from
  `resource."service.name"`), `trace_id`, `span_id`, `scope_name`, and
  `attributes` / `resource` as JSON text, plus `attr_<key>` for each key in
  `columnar.projection.attributes`. It reads both the flattened shape
  chronik-signal writes and the OTLP/JSON `LogRecord` names (`severityText`,
  `timeUnixNano`, `body.stringValue`, ...).
- **`none`** projects only declared columns.

**Declared columns** (`columnar.columns`) are added to the mode's columns, and
replace one of the same name. Each is `name[:type][:/path[|/path...]]` (no `=`: `kafka-configs.sh` refuses values containing one):

- `name`: a letter first, then letters, digits and `_` (the `_` prefix is the
  envelope's).
- `type`: `string`, `int64`, `float64`, `boolean`, `timestamp` (UTC,
  nanoseconds), `json` (the value's JSON text). Omitted: `string`.
- `path`: a JSON Pointer — `/resource/service.name` reads key `service.name`
  of object `resource`; `~1` stands for `/` and `~0` for `~`; a number indexes
  an array. Alternatives separated by `|` are tried in order and the first
  present, non-null one wins. Omitted: `/<name>`.

```bash
kafka-configs.sh --bootstrap-server localhost:9092 --alter --entity-type topics --entity-name default-logs \
  --add-config 'columnar.projection=otlp-logs,columnar.projection.attributes=http.status_code;user.id'

kafka-configs.sh --bootstrap-server localhost:9092 --alter --entity-type topics --entity-name orders \
  --add-config 'columnar.columns=region:string:/customer/address/region;amount:float64:/amount|/total'
```

An invalid declaration is refused by the admin call (`INVALID_CONFIG`, naming
the entry).

**Conversions.** Numbers and booleans read as text in a `string` column;
numeric strings read as numbers in `int64` / `float64` (OTLP/JSON writes 64-bit
integers as strings); a `timestamp` reads RFC 3339 text, or epoch time as a
number or numeric string, the unit taken from its magnitude (seconds below
1e11, milliseconds below 1e14, microseconds below 1e17, else nanoseconds).
Anything that does not convert is NULL. `_value` is never changed.

**History.** A column applies to every record of the topic, not just those
written after it was configured. Segments written earlier do not carry it;
for those, the scan reads `_value` and computes the column exactly as the
writer would have, so counts over the whole topic are complete. Only those
older segments pay for decoding `_value`. A column whose type changed reads
older segments through a cast (text to timestamp works for RFC 3339 text).

**Checking pushdown.** `POST /_sql/explain` returns the physical plan; a
filter on a projected column shows a Parquet scan whose `projection=[...]`
lists that column and not `_value`.

### Querying fields that are not projected

Any field can still be read from `_value` with the JSON functions:

```sql
SELECT json_extract_string(_value, '$.order_id') AS order_id FROM orders;
SELECT count(*) FROM orders WHERE json_extract_float(_value, '$.amount') > 100;
```

`json_extract_string`, `json_extract_int`, `json_extract_float` and
`json_extract` (JSON text) take `$.a.b` paths. `json_project_string`,
`json_project_int`, `json_project_float`, `json_project_bool`,
`json_project_timestamp` and `json_project_json` take a projected column's
pointer list (`'/resource/service.name'`) and convert exactly as projected
columns do. These decode `_value` row by row; a projected column does not.

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
- JSON: `json_extract_string`, `json_extract_int`, `json_extract_float`, `json_extract`, `json_project_*` (see Projected Columns)
- Time: `DATE_TRUNC`, `EXTRACT`, `TO_TIMESTAMP`, etc.

See [DataFusion SQL Reference](https://datafusion.apache.org/user-guide/sql/index.html) for full documentation.

---

## Related Documentation

- [Vector Search Guide](VECTOR_SEARCH_GUIDE.md) - Semantic search with embeddings
- [Disaster Recovery](DISASTER_RECOVERY.md) - Backup and restore
- [Advanced Storage Roadmap](ADVANCED_STORAGE_ROADMAP.md) - Feature roadmap
