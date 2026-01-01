# Columnar Storage Design: Per-Topic Arrow/Parquet Integration

> **NOTE**: This design document is part of the Advanced Storage Features project.
> See [ADVANCED_STORAGE_ROADMAP.md](ADVANCED_STORAGE_ROADMAP.md) for the unified implementation tracker
> that combines **Columnar Storage** + **Vector Search** as per-topic options.

## Overview

This document describes the design for adding optional per-topic columnar storage using Apache Arrow and Parquet, with a unified Query + Search API on a single port.

**Design Principles:**
1. **Per-topic opt-in** - Columnar storage is a topic-level configuration option
2. **Reliability first** - Arrow/Parquet for ecosystem compatibility and proven reliability
3. **Port consolidation** - Single unified API port for Search + Query + Admin
4. **Non-breaking** - Existing Kafka wire protocol and WAL remain unchanged
5. **Gradual adoption** - Topics can enable columnar storage at any time

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           CHRONIK STREAM v2.3.0                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
         ┌────────────────────────────┼────────────────────────────┐
         │                            │                            │
         ▼                            ▼                            ▼
┌─────────────────┐        ┌─────────────────┐        ┌─────────────────────┐
│  Kafka Protocol │        │  Unified API    │        │  Internal Services  │
│  (Port 9092)    │        │  (Port 6092)    │        │  (No external port) │
│  ─────────────  │        │  ─────────────  │        │  ─────────────────  │
│  Produce/Fetch  │        │  POST /_search  │        │  WAL Replication    │
│  Consumer Grps  │        │  POST /_sql     │        │  Raft Consensus     │
│  Metadata APIs  │        │  GET /admin/*   │        │  Background Tasks   │
│  CreateTopics   │        │  GET /schemas/* │        │                     │
└────────┬────────┘        └────────┬────────┘        └─────────────────────┘
         │                          │
         │                          ▼
         │               ┌──────────────────────────────────────────┐
         │               │           UNIFIED API ROUTER             │
         │               │  ──────────────────────────────────────  │
         │               │  /_search/*     → SearchHandler          │
         │               │  /_sql          → SqlQueryHandler        │
         │               │  /_arrow/*      → ArrowFlightHandler     │
         │               │  /admin/*       → AdminHandler           │
         │               │  /schemas/*     → SchemaRegistryHandler  │
         │               │  /metrics       → PrometheusHandler      │
         │               │  /health        → HealthHandler          │
         │               └──────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                          STORAGE LAYER                                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                      PER-TOPIC STORAGE CONFIG                        │   │
│  │                                                                      │   │
│  │  Topic: "orders"                    Topic: "logs"                   │   │
│  │  ├─ columnar.enabled: true          ├─ columnar.enabled: true       │   │
│  │  ├─ columnar.format: parquet        ├─ columnar.format: parquet     │   │
│  │  ├─ columnar.compression: zstd      ├─ columnar.compression: snappy │   │
│  │  ├─ columnar.row_group_size: 50000  ├─ columnar.row_group_size: 100k│   │
│  │  └─ columnar.schema: auto           └─ columnar.schema: explicit    │   │
│  │                                                                      │   │
│  │  Topic: "events"                    Topic: "metrics"                │   │
│  │  ├─ columnar.enabled: false         ├─ columnar.enabled: true       │   │
│  │  └─ (Kafka wire format only)        ├─ columnar.format: arrow       │   │
│  │                                     └─ columnar.partitioning: hourly│   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                      │                                      │
│         ┌────────────────────────────┼────────────────────────────┐        │
│         │                            │                            │        │
│         ▼                            ▼                            ▼        │
│  ┌─────────────────┐      ┌─────────────────┐      ┌─────────────────┐    │
│  │  HOT TIER       │      │  WARM TIER      │      │  COLD TIER      │    │
│  │  (In-Memory)    │      │  (Local Disk)   │      │  (Object Store) │    │
│  │  ─────────────  │      │  ─────────────  │      │  ─────────────  │    │
│  │  WAL + Buffers  │      │  Parquet Files  │      │  Parquet Files  │    │
│  │  Kafka Format   │  ──▶ │  + Tantivy Idx  │  ──▶ │  Partitioned    │    │
│  │  < 1 minute     │      │  1 min - 1 hour │      │  > 1 hour       │    │
│  │  Sub-ms reads   │      │  10-100ms reads │      │  100ms-1s reads │    │
│  └─────────────────┘      └─────────────────┘      └─────────────────┘    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Topic Configuration

### Extended TopicConfig

```rust
// In crates/chronik-common/src/metadata/traits.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub partition_count: u32,
    pub replication_factor: u32,
    pub retention_ms: Option<i64>,
    pub segment_bytes: i64,
    pub config: HashMap<String, String>,  // Extensible config
}

// Columnar configuration keys (stored in config HashMap):
//
// columnar.enabled          = "true" | "false"
// columnar.format           = "parquet" | "arrow" | "orc"
// columnar.compression      = "zstd" | "snappy" | "lz4" | "gzip" | "none"
// columnar.compression.level = "1" - "22" (for zstd)
// columnar.row_group_size   = "50000" - "1000000"
// columnar.page_size        = "1048576" (1MB default)
// columnar.schema           = "auto" | "explicit"
// columnar.schema.id        = "<schema-registry-id>" (if explicit)
// columnar.partitioning     = "none" | "hourly" | "daily"
// columnar.bloom_filter     = "true" | "false"
// columnar.bloom_filter.columns = "offset,timestamp,key"
// columnar.dictionary       = "true" | "false"
// columnar.dictionary.columns = "key,headers"
// columnar.statistics       = "full" | "chunk" | "none"
```

### Topic Creation with Columnar Options

```bash
# Via Kafka CLI (standard CreateTopics API)
kafka-topics.sh --create --topic orders \
  --partitions 6 \
  --replication-factor 3 \
  --config columnar.enabled=true \
  --config columnar.format=parquet \
  --config columnar.compression=zstd \
  --config columnar.row_group_size=100000

# Via Chronik Admin API
curl -X POST http://localhost:6092/admin/topics \
  -H "X-API-Key: $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "orders",
    "partitions": 6,
    "replication_factor": 3,
    "config": {
      "columnar.enabled": "true",
      "columnar.format": "parquet",
      "columnar.compression": "zstd",
      "columnar.row_group_size": "100000",
      "columnar.bloom_filter": "true",
      "columnar.bloom_filter.columns": "offset,timestamp"
    }
  }'
```

### Enable Columnar on Existing Topic

```bash
# Via Kafka AlterConfigs API
kafka-configs.sh --alter --topic orders \
  --add-config columnar.enabled=true,columnar.format=parquet

# Via Chronik Admin API
curl -X PATCH http://localhost:6092/admin/topics/orders/config \
  -H "X-API-Key: $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "columnar.enabled": "true",
    "columnar.format": "parquet"
  }'
```

---

## Unified API Port Design

### Port Consolidation

```
BEFORE (v2.2.x):
  Port 9092  - Kafka Protocol (binary)
  Port 6092  - Search API (REST)
  Port 10001 - Admin API + Schema Registry (REST)

AFTER (v2.3.0):
  Port 9092  - Kafka Protocol (binary) - unchanged
  Port 6092  - Unified API (REST + SQL + Arrow Flight) - same port, expanded capabilities
```

### Unified Router Structure

```rust
// In crates/chronik-server/src/unified_api/mod.rs

pub struct UnifiedApi {
    // Search capabilities
    search_api: Arc<SearchApi>,

    // SQL query engine (DataFusion)
    query_engine: Arc<QueryEngine>,

    // Arrow Flight for high-performance data transfer
    flight_service: Arc<FlightService>,

    // Admin operations
    admin_api: Arc<AdminApi>,

    // Schema Registry
    schema_registry: Arc<SchemaRegistry>,

    // Shared state
    metadata_store: Arc<dyn MetadataStore>,
    segment_index: Arc<SegmentIndex>,

    // Configuration
    config: UnifiedApiConfig,
}

impl UnifiedApi {
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            // Health & Monitoring (public)
            .route("/health", get(health_handler))
            .route("/health/ready", get(readiness_handler))
            .route("/health/live", get(liveness_handler))
            .route("/metrics", get(prometheus_metrics))

            // Elasticsearch-compatible Search API
            .route("/_search", get(search_all).post(search_all))
            .route("/:index/_search", get(search_index).post(search_index))
            .route("/:index/_doc/:id", get(get_doc).post(index_doc).delete(delete_doc))
            .route("/:index/_mapping", get(get_mapping))
            .route("/_cat/indices", get(cat_indices))

            // SQL Query API (new)
            .route("/_sql", post(sql_query))
            .route("/_sql/explain", post(sql_explain))
            .route("/_sql/tables", get(list_tables))
            .route("/_sql/describe/:table", get(describe_table))

            // Arrow Flight endpoint (high-performance data transfer)
            .route("/_arrow/query", post(arrow_query))
            .route("/_arrow/stream/:query_id", get(arrow_stream))

            // Admin API
            .nest("/admin", admin_routes())

            // Schema Registry (Confluent-compatible)
            .nest("/schemas", schema_registry_routes())
            .route("/subjects", get(list_subjects))
            .route("/subjects/:subject/versions", get(list_versions).post(register_schema))
            .route("/subjects/:subject/versions/:version", get(get_schema))

            // Middleware
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http())
            .layer(CompressionLayer::new())
            .with_state(self)
    }
}
```

### SQL Query Endpoint

```rust
// POST /_sql
#[derive(Deserialize)]
struct SqlRequest {
    query: String,

    #[serde(default)]
    format: OutputFormat,  // json, arrow, csv

    #[serde(default = "default_limit")]
    limit: usize,

    #[serde(default)]
    timeout_ms: Option<u64>,

    #[serde(default)]
    parameters: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
struct SqlResponse {
    columns: Vec<ColumnInfo>,
    rows: Vec<Vec<serde_json::Value>>,
    row_count: usize,
    execution_time_ms: u64,

    #[serde(skip_serializing_if = "Option::is_none")]
    next_page_token: Option<String>,
}

async fn sql_query(
    State(api): State<Arc<UnifiedApi>>,
    Json(request): Json<SqlRequest>,
) -> Result<Json<SqlResponse>, ApiError> {
    // Parse and validate SQL
    let plan = api.query_engine.create_logical_plan(&request.query)?;

    // Execute against Parquet segments
    let results = api.query_engine.execute(plan, request.limit).await?;

    Ok(Json(results.into()))
}
```

### Example SQL Queries

```sql
-- Query orders by time range
SELECT offset, timestamp, key, value
FROM orders
WHERE timestamp BETWEEN '2024-01-01' AND '2024-01-31'
  AND partition = 0
ORDER BY timestamp DESC
LIMIT 100;

-- Aggregate message counts by hour
SELECT
    date_trunc('hour', to_timestamp(timestamp / 1000)) as hour,
    partition,
    COUNT(*) as msg_count,
    SUM(length(value)) as total_bytes,
    AVG(length(value)) as avg_msg_size
FROM logs
WHERE timestamp > now() - interval '24 hours'
GROUP BY 1, 2
ORDER BY 1 DESC;

-- Join across topics (advanced)
SELECT o.key as order_id, o.value as order_data, p.value as payment_data
FROM orders o
JOIN payments p ON o.key = p.key
WHERE o.timestamp > now() - interval '1 hour';

-- Full-text search on JSON values
SELECT offset, json_extract(value, '$.customer_id') as customer_id
FROM orders
WHERE json_extract_string(value, '$.status') = 'pending';
```

---

## Arrow/Parquet Implementation

### New Crate: `chronik-columnar`

```
crates/chronik-columnar/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── config.rs           # ColumnarConfig, validation
│   ├── schema.rs            # Arrow schema from topic config
│   ├── writer.rs            # ParquetWriter, batch conversion
│   ├── reader.rs            # ParquetReader, predicate pushdown
│   ├── converter.rs         # CanonicalRecord <-> Arrow
│   ├── partitioner.rs       # Time-based partitioning
│   ├── query_engine.rs      # DataFusion integration
│   ├── flight.rs            # Arrow Flight service
│   └── compaction.rs        # Parquet file compaction
```

### Dependencies

```toml
# In crates/chronik-columnar/Cargo.toml
[dependencies]
# Arrow ecosystem (use same version for compatibility)
arrow = { version = "54", default-features = false, features = ["ffi", "prettyprint"] }
arrow-schema = "54"
arrow-array = "54"
arrow-buffer = "54"
arrow-cast = "54"
arrow-json = "54"
parquet = { version = "54", features = ["snap", "zstd", "lz4", "async"] }

# SQL query engine
datafusion = "44"
datafusion-common = "44"
datafusion-expr = "44"
datafusion-sql = "44"

# Arrow Flight for streaming
arrow-flight = { version = "54", features = ["flight-sql-experimental"] }
tonic = { workspace = true }

# Internal dependencies
chronik-common = { path = "../chronik-common" }
chronik-storage = { path = "../chronik-storage" }
chronik-wal = { path = "../chronik-wal" }

# Async
tokio = { workspace = true }
futures = { workspace = true }
async-trait = { workspace = true }

# Utils
bytes = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
```

### Arrow Schema for Kafka Messages

```rust
// In crates/chronik-columnar/src/schema.rs

use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use std::sync::Arc;

/// Standard Arrow schema for Kafka messages
pub fn kafka_message_schema() -> Schema {
    Schema::new(vec![
        // Metadata columns (always present)
        Field::new("_topic", DataType::Utf8, false),
        Field::new("_partition", DataType::Int32, false),
        Field::new("_offset", DataType::Int64, false),
        Field::new("_timestamp", DataType::Timestamp(TimeUnit::Millisecond, None), false),
        Field::new("_timestamp_type", DataType::Int8, false),  // 0=CreateTime, 1=LogAppendTime

        // Key (nullable binary)
        Field::new("_key", DataType::Binary, true),
        Field::new("_key_string", DataType::Utf8, true),  // Decoded if UTF-8

        // Value (binary, with optional decoded fields)
        Field::new("_value", DataType::Binary, false),
        Field::new("_value_size", DataType::Int32, false),

        // Headers as Map
        Field::new(
            "_headers",
            DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(vec![
                        Field::new("key", DataType::Utf8, false).into(),
                        Field::new("value", DataType::Binary, true).into(),
                    ].into()),
                    false,
                )),
                false,  // keys_sorted
            ),
            true,  // nullable
        ),

        // Producer metadata
        Field::new("_producer_id", DataType::Int64, true),
        Field::new("_producer_epoch", DataType::Int16, true),
        Field::new("_sequence", DataType::Int32, true),
    ])
}

/// Extended schema with decoded JSON fields
pub fn extended_schema_with_json_fields(
    base: Schema,
    json_fields: Vec<JsonFieldConfig>,
) -> Schema {
    let mut fields = base.fields().to_vec();

    for json_field in json_fields {
        let arrow_type = json_type_to_arrow(&json_field.json_type);
        fields.push(Arc::new(Field::new(
            &json_field.name,
            arrow_type,
            json_field.nullable,
        )));
    }

    Schema::new(fields)
}

/// Configuration for extracting JSON fields into columns
#[derive(Debug, Clone)]
pub struct JsonFieldConfig {
    pub name: String,           // Column name in Arrow
    pub json_path: String,      // JSONPath expression (e.g., "$.customer.id")
    pub json_type: JsonType,    // Expected type
    pub nullable: bool,
}

#[derive(Debug, Clone)]
pub enum JsonType {
    String,
    Int64,
    Float64,
    Boolean,
    Timestamp,
    Array(Box<JsonType>),
    Object,
}
```

### Parquet Writer

```rust
// In crates/chronik-columnar/src/writer.rs

use arrow::array::*;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::{WriterProperties, WriterVersion};
use parquet::basic::{Compression, Encoding};
use std::fs::File;
use std::sync::Arc;

pub struct ParquetSegmentWriter {
    config: ColumnarConfig,
    schema: Arc<Schema>,
    writer: Option<ArrowWriter<File>>,
    current_row_group: Vec<RecordBatch>,
    current_row_count: usize,
    file_path: PathBuf,
}

impl ParquetSegmentWriter {
    pub fn new(config: ColumnarConfig, schema: Arc<Schema>, path: PathBuf) -> Result<Self> {
        let file = File::create(&path)?;

        let props = WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_2_0)
            .set_compression(config.compression.into())
            .set_dictionary_enabled(config.dictionary_enabled)
            .set_statistics_enabled(config.statistics.into())
            .set_max_row_group_size(config.row_group_size)
            .set_data_page_size_limit(config.page_size)
            // Enable bloom filters for specified columns
            .set_bloom_filter_enabled(true)
            .build();

        let writer = ArrowWriter::try_new(file, schema.clone(), Some(props))?;

        Ok(Self {
            config,
            schema,
            writer: Some(writer),
            current_row_group: Vec::new(),
            current_row_count: 0,
            file_path: path,
        })
    }

    /// Convert CanonicalRecords to Arrow RecordBatch and write
    pub fn write_records(&mut self, records: &[CanonicalRecord]) -> Result<()> {
        let batch = self.records_to_batch(records)?;
        self.write_batch(batch)
    }

    fn records_to_batch(&self, records: &[CanonicalRecord]) -> Result<RecordBatch> {
        let len = records.len();

        // Build arrays for each column
        let mut topic_builder = StringBuilder::with_capacity(len, len * 32);
        let mut partition_builder = Int32Builder::with_capacity(len);
        let mut offset_builder = Int64Builder::with_capacity(len);
        let mut timestamp_builder = TimestampMillisecondBuilder::with_capacity(len);
        let mut key_builder = BinaryBuilder::with_capacity(len, len * 64);
        let mut value_builder = BinaryBuilder::with_capacity(len, len * 1024);
        // ... more builders for other columns

        for record in records {
            for entry in &record.entries {
                topic_builder.append_value(&record.topic);
                partition_builder.append_value(record.partition);
                offset_builder.append_value(entry.offset);
                timestamp_builder.append_value(entry.timestamp);

                if let Some(key) = &entry.key {
                    key_builder.append_value(key);
                } else {
                    key_builder.append_null();
                }

                value_builder.append_value(&entry.value);
            }
        }

        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::new(topic_builder.finish()),
                Arc::new(partition_builder.finish()),
                Arc::new(offset_builder.finish()),
                Arc::new(timestamp_builder.finish()),
                Arc::new(key_builder.finish()),
                Arc::new(value_builder.finish()),
                // ... more arrays
            ],
        )?;

        Ok(batch)
    }

    fn write_batch(&mut self, batch: RecordBatch) -> Result<()> {
        self.current_row_count += batch.num_rows();
        self.current_row_group.push(batch);

        // Flush row group when threshold reached
        if self.current_row_count >= self.config.row_group_size {
            self.flush_row_group()?;
        }

        Ok(())
    }

    fn flush_row_group(&mut self) -> Result<()> {
        if let Some(writer) = &mut self.writer {
            for batch in self.current_row_group.drain(..) {
                writer.write(&batch)?;
            }
            self.current_row_count = 0;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<ParquetFileMetadata> {
        self.flush_row_group()?;

        if let Some(writer) = self.writer.take() {
            let metadata = writer.close()?;

            Ok(ParquetFileMetadata {
                path: self.file_path,
                row_count: metadata.num_rows as u64,
                file_size: metadata.file_size() as u64,
                row_groups: metadata.num_row_groups(),
                created_by: metadata.created_by().map(|s| s.to_string()),
            })
        } else {
            Err(anyhow!("Writer already closed"))
        }
    }
}
```

### Query Engine (DataFusion)

```rust
// In crates/chronik-columnar/src/query_engine.rs

use datafusion::prelude::*;
use datafusion::datasource::listing::{ListingTable, ListingTableConfig, ListingOptions};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use std::sync::Arc;

pub struct ColumnarQueryEngine {
    ctx: SessionContext,
    metadata_store: Arc<dyn MetadataStore>,
    segment_index: Arc<SegmentIndex>,
    config: QueryEngineConfig,
}

impl ColumnarQueryEngine {
    pub async fn new(
        metadata_store: Arc<dyn MetadataStore>,
        segment_index: Arc<SegmentIndex>,
        config: QueryEngineConfig,
    ) -> Result<Self> {
        let mut ctx = SessionContext::new();

        // Register UDFs for Kafka-specific operations
        ctx.register_udf(create_json_extract_udf());
        ctx.register_udf(create_json_extract_string_udf());
        ctx.register_udf(create_decode_avro_udf());
        ctx.register_udf(create_decode_protobuf_udf());

        Ok(Self {
            ctx,
            metadata_store,
            segment_index,
            config,
        })
    }

    /// Register a topic as a queryable table
    pub async fn register_topic(&self, topic: &str) -> Result<()> {
        let topic_meta = self.metadata_store.get_topic(topic).await?
            .ok_or_else(|| anyhow!("Topic not found: {}", topic))?;

        // Check if columnar is enabled for this topic
        let columnar_enabled = topic_meta.config.config
            .get("columnar.enabled")
            .map(|v| v == "true")
            .unwrap_or(false);

        if !columnar_enabled {
            return Err(anyhow!("Columnar storage not enabled for topic: {}", topic));
        }

        // Get Parquet file paths for this topic
        let parquet_paths = self.segment_index.get_parquet_paths(topic).await?;

        if parquet_paths.is_empty() {
            // No Parquet files yet - topic exists but no data converted
            return Ok(());
        }

        // Create listing table config
        let format = ParquetFormat::default()
            .with_enable_pruning(true)
            .with_pushdown_filters(true)
            .with_reorder_filters(true);

        let listing_options = ListingOptions::new(Arc::new(format))
            .with_file_extension(".parquet")
            .with_collect_stat(true);

        let config = ListingTableConfig::new_with_multi_paths(parquet_paths)
            .with_listing_options(listing_options)
            .infer_schema(&self.ctx.state())
            .await?;

        let table = ListingTable::try_new(config)?;

        self.ctx.register_table(topic, Arc::new(table))?;

        Ok(())
    }

    /// Execute SQL query
    pub async fn execute_sql(&self, sql: &str) -> Result<Vec<RecordBatch>> {
        // Parse and create logical plan
        let plan = self.ctx.state().create_logical_plan(sql).await?;

        // Optimize
        let optimized = self.ctx.state().optimize(&plan)?;

        // Execute
        let physical = self.ctx.state().create_physical_plan(&optimized).await?;
        let results = collect(physical, self.ctx.task_ctx()).await?;

        Ok(results)
    }

    /// Execute with streaming results
    pub async fn execute_sql_stream(
        &self,
        sql: &str
    ) -> Result<SendableRecordBatchStream> {
        let df = self.ctx.sql(sql).await?;
        Ok(df.execute_stream().await?)
    }

    /// Explain query plan
    pub async fn explain(&self, sql: &str, analyze: bool) -> Result<String> {
        let df = self.ctx.sql(sql).await?;
        let plan = if analyze {
            df.explain(false, true)?.collect().await?
        } else {
            df.explain(false, false)?.collect().await?
        };

        Ok(format_batches(&plan)?)
    }
}
```

---

## Integration with Existing Storage

### Modified WalIndexer

```rust
// In crates/chronik-storage/src/wal_indexer.rs (modifications)

impl WalIndexer {
    async fn process_sealed_segment(&self, segment: WalSegment) -> Result<()> {
        let records = self.read_segment_records(&segment).await?;

        // Group records by topic
        let by_topic = self.group_by_topic(&records);

        for (topic, topic_records) in by_topic {
            let topic_config = self.metadata_store.get_topic(&topic).await?;

            // 1. Always write Tantivy index (if searchable)
            if self.is_searchable(&topic_config) {
                self.write_tantivy_index(&topic, &topic_records).await?;
            }

            // 2. Optionally write Parquet (if columnar enabled)
            if self.is_columnar_enabled(&topic_config) {
                self.write_parquet_segment(&topic, &topic_records, &topic_config).await?;
            }
        }

        Ok(())
    }

    fn is_columnar_enabled(&self, config: &Option<TopicMetadata>) -> bool {
        config.as_ref()
            .and_then(|c| c.config.config.get("columnar.enabled"))
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    async fn write_parquet_segment(
        &self,
        topic: &str,
        records: &[CanonicalRecord],
        config: &TopicMetadata,
    ) -> Result<()> {
        let columnar_config = ColumnarConfig::from_topic_config(&config.config)?;

        // Determine output path based on partitioning
        let path = self.parquet_path(topic, records, &columnar_config);

        // Get or create schema (auto-infer or from schema registry)
        let schema = self.get_parquet_schema(topic, &columnar_config).await?;

        // Write Parquet file
        let mut writer = ParquetSegmentWriter::new(columnar_config, schema, path.clone())?;
        writer.write_records(records)?;
        let metadata = writer.finish()?;

        // Register in segment index
        self.segment_index.register_parquet(topic, &path, metadata).await?;

        // Upload to object store if configured
        if self.should_upload_to_cold_tier(&columnar_config) {
            self.upload_parquet_to_object_store(&path).await?;
        }

        info!(
            topic = %topic,
            path = %path.display(),
            rows = metadata.row_count,
            size = metadata.file_size,
            "Wrote Parquet segment"
        );

        Ok(())
    }
}
```

### Segment Index Extension

```rust
// In crates/chronik-storage/src/segment_index.rs (additions)

impl SegmentIndex {
    /// Get Parquet file paths for a topic
    pub async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>> {
        let parquet_segments = self.parquet_segments.read().await;

        Ok(parquet_segments
            .get(topic)
            .map(|segments| segments.iter().map(|s| s.path.clone()).collect())
            .unwrap_or_default())
    }

    /// Register a new Parquet segment
    pub async fn register_parquet(
        &self,
        topic: &str,
        path: &Path,
        metadata: ParquetFileMetadata,
    ) -> Result<()> {
        let mut parquet_segments = self.parquet_segments.write().await;

        let segment = ParquetSegmentInfo {
            path: path.to_string_lossy().to_string(),
            metadata,
            created_at: Utc::now(),
        };

        parquet_segments
            .entry(topic.to_string())
            .or_default()
            .push(segment);

        // Persist to metadata WAL
        self.persist_parquet_index().await?;

        Ok(())
    }
}
```

---

## Configuration

### Server Configuration

```toml
# chronik.toml

[server]
kafka_port = 9092
unified_api_port = 6092  # Unified API port (same as Search API for backward compatibility)

[unified_api]
# Enable/disable API sections
search_enabled = true
sql_enabled = true
arrow_flight_enabled = true
admin_enabled = true
schema_registry_enabled = true

# Request limits
max_request_size = "100MB"
request_timeout_ms = 30000

# SQL query limits
sql_max_rows = 100000
sql_max_concurrent_queries = 100

[columnar]
# Global defaults (can be overridden per-topic)
enabled = false  # Opt-in at topic level
format = "parquet"
compression = "zstd"
compression_level = 3
row_group_size = 100000
page_size = 1048576  # 1MB
dictionary_enabled = true
bloom_filter_enabled = true
statistics = "full"

# Partitioning
partitioning = "hourly"  # none, hourly, daily

# Retention for Parquet files
parquet_retention_hours = 168  # 7 days (separate from Kafka retention)

# Object store for cold tier
[columnar.object_store]
type = "s3"  # s3, gcs, azure, local
bucket = "chronik-parquet"
prefix = "topics/"
```

### Environment Variables

```bash
# Unified API port
CHRONIK_UNIFIED_API_PORT=6092

# Enable columnar globally (still per-topic opt-in)
CHRONIK_COLUMNAR_DEFAULT_ENABLED=false

# Parquet defaults
CHRONIK_COLUMNAR_FORMAT=parquet
CHRONIK_COLUMNAR_COMPRESSION=zstd
CHRONIK_COLUMNAR_ROW_GROUP_SIZE=100000
```

---

## API Examples

### SQL Query via REST

```bash
# Simple query
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{
    "query": "SELECT * FROM orders WHERE timestamp > now() - interval '\''1 hour'\'' LIMIT 10",
    "format": "json"
  }'

# Response
{
  "columns": [
    {"name": "_topic", "type": "Utf8"},
    {"name": "_partition", "type": "Int32"},
    {"name": "_offset", "type": "Int64"},
    {"name": "_timestamp", "type": "Timestamp(Millisecond, None)"},
    {"name": "_key", "type": "Binary"},
    {"name": "_value", "type": "Binary"}
  ],
  "rows": [
    ["orders", 0, 12345, "2024-01-15T10:30:00Z", "b64:...", "b64:..."],
    ["orders", 2, 12346, "2024-01-15T10:30:01Z", "b64:...", "b64:..."]
  ],
  "row_count": 2,
  "execution_time_ms": 45
}
```

### Arrow Flight (High-Performance)

```python
# Python client using pyarrow
import pyarrow.flight as flight

# Connect to Chronik
client = flight.connect("grpc://localhost:6092")

# Execute query and get Arrow stream
info = client.get_flight_info(
    flight.FlightDescriptor.for_command(
        b'SELECT * FROM orders WHERE partition = 0 LIMIT 1000'
    )
)

# Read results as Arrow tables (zero-copy)
reader = client.do_get(info.endpoints[0].ticket)
table = reader.read_all()

# Convert to pandas
df = table.to_pandas()
```

### Combined Search + SQL

```bash
# First, full-text search to find relevant offsets
curl -X POST http://localhost:6092/orders/_search \
  -H "Content-Type: application/json" \
  -d '{
    "query": {"match": {"_value": "refund"}},
    "size": 100
  }'

# Then, SQL query for analytics on those results
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{
    "query": "SELECT date_trunc('\''day'\'', _timestamp) as day, COUNT(*) as refund_count FROM orders WHERE _offset IN (12345, 12346, 12347) GROUP BY 1"
  }'
```

---

## Implementation Phases

### Phase 1: Foundation (Week 1-2)

**Tasks:**
1. Create `chronik-columnar` crate with Arrow/Parquet dependencies
2. Implement `ColumnarConfig` and validation
3. Extend `TopicConfig` with columnar options
4. Add columnar config validation in `TopicValidator`
5. Implement basic `ParquetSegmentWriter`

**Deliverables:**
- Topics can be created with `columnar.enabled=true`
- Configuration stored in metadata WAL
- No Parquet files written yet

### Phase 2: Write Path (Week 2-3)

**Tasks:**
1. Implement `CanonicalRecord` → Arrow conversion
2. Integrate `ParquetSegmentWriter` with `WalIndexer`
3. Add Parquet file path management in `SegmentIndex`
4. Implement time-based partitioning
5. Add bloom filters and statistics

**Deliverables:**
- Topics with columnar enabled produce Parquet files
- Files stored locally and optionally uploaded to object store

### Phase 3: Query Engine (Week 3-4)

**Tasks:**
1. Integrate DataFusion
2. Implement `ColumnarQueryEngine` with topic registration
3. Add SQL endpoint to unified API
4. Implement predicate pushdown
5. Add query explain functionality

**Deliverables:**
- SQL queries work against Parquet files
- Predicate pushdown for efficient filtering

### Phase 4: Unified API (Week 4)

**Tasks:**
1. Create `UnifiedApi` struct merging Search + Query + Admin
2. Migrate Search API endpoints
3. Migrate Admin API endpoints
4. Add SQL endpoints
5. Add Arrow Flight endpoint
6. Single port configuration

**Deliverables:**
- All APIs on single port 6092 (backward compatible)
- Backward-compatible endpoint paths

### Phase 5: Polish & Testing (Week 5)

**Tasks:**
1. Comprehensive integration tests
2. Performance benchmarks
3. Documentation
4. Migration guide from v2.2.x

**Deliverables:**
- Production-ready release
- Performance comparison vs. non-columnar

---

## Performance Expectations

| Query Type | Without Columnar | With Parquet | Improvement |
|------------|------------------|--------------|-------------|
| Offset range (1K msgs) | 5-10ms | 5-10ms | Same (hot path) |
| Timestamp range (1M msgs) | 500ms-2s | 50-100ms | 10-20x |
| Aggregation (1M msgs) | N/A | 100-200ms | New capability |
| Full-text + SQL join | N/A | 200-500ms | New capability |
| Full scan (10M msgs) | Not feasible | 1-2s | New capability |

**Storage Efficiency:**

| Compression | Kafka Format | Parquet | Savings |
|-------------|--------------|---------|---------|
| None | 100% | 40-50% | 50-60% |
| Snappy | 60% | 30-35% | 50-60% |
| Zstd | 40% | 20-25% | 50-60% |

---

## Migration Guide

### Enabling Columnar on Existing Topics

```bash
# 1. Enable columnar (new data only)
kafka-configs.sh --alter --topic orders \
  --add-config columnar.enabled=true

# 2. Backfill historical data (optional)
chronik-cli columnar backfill --topic orders --from-offset 0

# 3. Verify
curl http://localhost:6092/_sql -d '{"query": "SELECT COUNT(*) FROM orders"}'
```

### Port Migration

```bash
# Old configuration (v2.2.x)
# Port 9092 - Kafka
# Port 6092 - Search
# Port 10001 - Admin

# New configuration (v2.3.0)
# Port 9092 - Kafka (unchanged)
# Port 6092 - Unified (Search + SQL + Admin + Schema Registry)
# Same port as before - existing Search API clients work unchanged
# Admin API moved from 10001 to 6092/admin/*
```

---

## Files to Create/Modify

### New Files

| File | Purpose |
|------|---------|
| `crates/chronik-columnar/Cargo.toml` | New crate dependencies |
| `crates/chronik-columnar/src/lib.rs` | Module exports |
| `crates/chronik-columnar/src/config.rs` | ColumnarConfig struct |
| `crates/chronik-columnar/src/schema.rs` | Arrow schema definitions |
| `crates/chronik-columnar/src/writer.rs` | ParquetSegmentWriter |
| `crates/chronik-columnar/src/reader.rs` | ParquetSegmentReader |
| `crates/chronik-columnar/src/converter.rs` | Record ↔ Arrow conversion |
| `crates/chronik-columnar/src/query_engine.rs` | DataFusion integration |
| `crates/chronik-server/src/unified_api/mod.rs` | Unified API router |
| `crates/chronik-server/src/unified_api/sql_handler.rs` | SQL endpoint |

### Modified Files

| File | Change |
|------|--------|
| `Cargo.toml` | Add chronik-columnar to workspace |
| `crates/chronik-common/src/metadata/traits.rs` | Document columnar config keys |
| `crates/chronik-protocol/src/create_topics/topic_validator.rs` | Validate columnar configs |
| `crates/chronik-storage/src/wal_indexer.rs` | Add Parquet writing |
| `crates/chronik-storage/src/segment_index.rs` | Track Parquet files |
| `crates/chronik-server/src/main.rs` | Start unified API |
| `crates/chronik-server/Cargo.toml` | Add chronik-columnar dependency |

---

## Summary

This design provides:

1. **Per-topic columnar storage** via `TopicConfig.config` HashMap (existing infrastructure)
2. **Arrow/Parquet format** for reliability and ecosystem compatibility
3. **Unified API on single port** (6092) for Search + SQL + Admin + Schema Registry
4. **Non-breaking changes** - existing Kafka protocol and WAL unchanged
5. **Gradual adoption** - topics opt-in to columnar storage individually
6. **Full SQL support** via DataFusion with predicate pushdown
7. **High-performance data transfer** via Arrow Flight

The implementation leverages existing patterns (WalIndexer, SegmentIndex, Axum routers) while adding new capabilities without disrupting current functionality.
