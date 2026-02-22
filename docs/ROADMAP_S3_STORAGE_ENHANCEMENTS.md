# Roadmap: S3 Storage Enhancements

## Overview

This roadmap covers three major S3 storage enhancements:

1. **Per-Topic S3 Bucket Configuration** - Individual S3 buckets per topic
2. **Diskless Topics** - Topics that use S3 directly without local disk
3. **External Parquet Integration** - Use existing Parquet files as topic data source

## Current State (v2.2.23)

```
┌─────────────────────────────────────────────────────────────────┐
│                     Current Architecture                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Producer → WAL (local disk) → Segments → Global S3 Bucket      │
│                 ↓                              ↓                 │
│            Parquet files              tantivy_indexes            │
│         (local + optional S3)        (local + optional S3)       │
│                                                                 │
│  Limitations:                                                   │
│  - Single S3 bucket for all topics                              │
│  - Local disk always required (WAL)                             │
│  - Cannot use existing Parquet files                            │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Target State

```
┌─────────────────────────────────────────────────────────────────┐
│                      Target Architecture                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Topic A (Standard):                                            │
│    Producer → WAL (local) → S3 Bucket A                         │
│                                                                 │
│  Topic B (Diskless):                                            │
│    Producer → S3 Bucket B (direct write, no local disk)         │
│                                                                 │
│  Topic C (External Parquet):                                    │
│    Read-only from s3://data-lake/events/*.parquet               │
│                                                                 │
│  Topic D (Hybrid):                                              │
│    WAL (local) + External Parquet (read) + New Parquet (write)  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Phase 1: Per-Topic S3 Configuration

### Goal
Allow each topic to have its own S3 bucket, credentials, and storage settings.

### Topic Configuration Options

```bash
# Create topic with custom S3 bucket
kafka-topics.sh --create --topic financial-data \
  --bootstrap-server localhost:9092 \
  --config s3.bucket=company-financial-data \
  --config s3.region=us-east-1 \
  --config s3.prefix=kafka/financial \
  --config s3.access.key.id=AKIA... \
  --config s3.secret.access.key=secret... \
  --config s3.endpoint=https://s3.amazonaws.com

# Or use IAM role (no explicit credentials)
kafka-topics.sh --create --topic logs \
  --config s3.bucket=company-logs \
  --config s3.region=eu-west-1 \
  --config s3.use.iam.role=true
```

### Implementation Tasks

#### 1.1 Topic S3 Config Schema
**File:** `crates/chronik-protocol/src/create_topics/topic_validator.rs`

```rust
// New S3 config keys
const S3_CONFIG_KEYS: &[&str] = &[
    "s3.enabled",              // Enable per-topic S3 (default: inherit from server)
    "s3.bucket",               // S3 bucket name (required if s3.enabled=true)
    "s3.region",               // AWS region (default: server default)
    "s3.prefix",               // Object key prefix (default: topic name)
    "s3.endpoint",             // Custom endpoint for MinIO/LocalStack
    "s3.access.key.id",        // AWS access key (optional, uses IAM if not set)
    "s3.secret.access.key",    // AWS secret key
    "s3.use.iam.role",         // Use IAM role instead of credentials
    "s3.storage.class",        // STANDARD, INTELLIGENT_TIERING, GLACIER, etc.
    "s3.encryption",           // SSE-S3, SSE-KMS, or none
    "s3.kms.key.id",           // KMS key for SSE-KMS
];
```

#### 1.2 S3 Client Pool
**File:** `crates/chronik-storage/src/s3_client_pool.rs` (new)

```rust
/// Pool of S3 clients, one per unique bucket/region/credentials combination
pub struct S3ClientPool {
    clients: DashMap<S3ClientKey, Arc<S3Client>>,
    default_client: Arc<S3Client>,
}

#[derive(Hash, Eq, PartialEq)]
struct S3ClientKey {
    bucket: String,
    region: String,
    endpoint: Option<String>,
    // Credentials hash (not stored directly for security)
    credentials_hash: u64,
}

impl S3ClientPool {
    /// Get or create S3 client for topic
    pub async fn get_client(&self, topic_config: &TopicS3Config) -> Arc<S3Client> {
        // Return cached client or create new one
    }
}
```

#### 1.3 Storage Router
**File:** `crates/chronik-storage/src/storage_router.rs` (new)

```rust
/// Routes storage operations to correct S3 bucket based on topic config
pub struct StorageRouter {
    s3_pool: Arc<S3ClientPool>,
    metadata_store: Arc<dyn MetadataStore>,
    topic_configs: DashMap<String, TopicS3Config>,
}

impl StorageRouter {
    /// Get object store for topic
    pub async fn get_store(&self, topic: &str) -> Arc<dyn ObjectStore> {
        if let Some(config) = self.topic_configs.get(topic) {
            self.s3_pool.get_client(&config).await
        } else {
            self.s3_pool.default_client.clone()
        }
    }

    /// Write segment to topic's S3 bucket
    pub async fn write_segment(&self, topic: &str, segment: &Segment) -> Result<()>;

    /// Read segment from topic's S3 bucket
    pub async fn read_segment(&self, topic: &str, segment_id: &str) -> Result<Segment>;
}
```

#### 1.4 Metadata Updates
**File:** `crates/chronik-common/src/metadata/mod.rs`

```rust
/// Topic S3 configuration stored in metadata
#[derive(Clone, Serialize, Deserialize)]
pub struct TopicS3Config {
    pub enabled: bool,
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub storage_class: S3StorageClass,
    pub encryption: S3Encryption,
    // Credentials stored in separate secure storage (not in metadata)
    pub credentials_ref: Option<String>,
}
```

### Estimated Effort: 3-4 days

---

## Phase 2: Diskless Topics

### Goal
Topics that write directly to S3 without using local disk, enabling:
- Infinite retention without local storage limits
- Lower cost for archival/compliance data
- Simpler deployment (no persistent volumes needed)

### Topic Configuration

```bash
# Create diskless topic
kafka-topics.sh --create --topic audit-logs \
  --bootstrap-server localhost:9092 \
  --config storage.mode=diskless \
  --config s3.bucket=audit-archive \
  --config s3.region=us-east-1

# Diskless with specific format
kafka-topics.sh --create --topic events \
  --config storage.mode=diskless \
  --config diskless.format=parquet \
  --config diskless.flush.interval.ms=60000 \
  --config diskless.flush.records=10000
```

### Storage Modes

| Mode | WAL | Local Segments | S3 | Latency | Use Case |
|------|-----|----------------|----|---------|-----------|
| `standard` (default) | Yes | Yes | Optional | Low (<10ms) | Real-time streaming |
| `tiered` | Yes | Yes | Yes | Low + archival | Hot/warm/cold data |
| `diskless` | No | No | Yes | Medium (50-200ms) | Archive, compliance |
| `external` | No | No | Read-only | Medium | Data lake integration |

### Implementation Tasks

#### 2.1 Diskless Producer
**File:** `crates/chronik-server/src/diskless_producer.rs` (new)

```rust
/// Producer that writes directly to S3 without local WAL
pub struct DisklessProducer {
    s3_client: Arc<S3Client>,
    buffer: Arc<RwLock<RecordBuffer>>,
    flush_config: DisklessFlushConfig,
}

struct RecordBuffer {
    records: Vec<KafkaRecord>,
    first_offset: i64,
    first_timestamp: i64,
    byte_size: usize,
}

impl DisklessProducer {
    /// Buffer record and flush to S3 when threshold reached
    pub async fn produce(&self, record: KafkaRecord) -> Result<ProduceResult> {
        let mut buffer = self.buffer.write().await;
        buffer.records.push(record);
        buffer.byte_size += record.size();

        if self.should_flush(&buffer) {
            self.flush_to_s3(&mut buffer).await?;
        }

        Ok(ProduceResult { offset, timestamp })
    }

    /// Flush buffer to S3 as Parquet/segment file
    async fn flush_to_s3(&self, buffer: &mut RecordBuffer) -> Result<()> {
        let parquet_bytes = self.records_to_parquet(&buffer.records)?;
        let key = format!(
            "{}/partition={}/{:020}-{:020}.parquet",
            self.topic, self.partition, buffer.first_offset, buffer.last_offset()
        );
        self.s3_client.put_object(&key, parquet_bytes).await?;
        buffer.clear();
        Ok(())
    }
}
```

#### 2.2 Diskless Consumer
**File:** `crates/chronik-server/src/diskless_consumer.rs` (new)

```rust
/// Consumer that reads directly from S3
pub struct DisklessConsumer {
    s3_client: Arc<S3Client>,
    segment_cache: Arc<SegmentCache>,
}

impl DisklessConsumer {
    /// Fetch records from S3
    pub async fn fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        // 1. Find segment containing offset
        let segment = self.find_segment(topic, partition, offset).await?;

        // 2. Download segment (with caching)
        let data = self.segment_cache.get_or_download(&segment).await?;

        // 3. Extract records from offset
        let records = self.extract_records(&data, offset, max_bytes)?;

        Ok(FetchResponse { records })
    }
}
```

#### 2.3 Flush Configuration
```rust
#[derive(Clone)]
pub struct DisklessFlushConfig {
    /// Flush after this many records
    pub records_threshold: usize,  // default: 10,000
    /// Flush after this many bytes
    pub bytes_threshold: usize,    // default: 64MB
    /// Flush after this duration
    pub time_threshold: Duration,  // default: 60s
    /// Output format
    pub format: DisklessFormat,    // Parquet, Segment, JSON
    /// Compression
    pub compression: Compression,  // ZSTD, Snappy, None
}
```

#### 2.4 Latency Considerations

```
┌─────────────────────────────────────────────────────────────────┐
│                    Diskless Latency Profile                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  PRODUCE:                                                       │
│    Buffer in memory → Ack immediately (< 1ms)                   │
│    Background flush to S3 (50-200ms, async)                     │
│                                                                 │
│  FETCH (cold):                                                  │
│    List S3 objects → Download segment → Parse → Return          │
│    ~100-500ms first request                                     │
│                                                                 │
│  FETCH (cached):                                                │
│    Hit segment cache → Parse → Return                           │
│    ~10-50ms                                                     │
│                                                                 │
│  Trade-off:                                                     │
│    ✓ Infinite retention, no disk management                    │
│    ✗ Higher latency than local WAL                             │
│    ✗ Data loss window = flush interval (mitigated by acks)     │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Estimated Effort: 5-7 days

---

## Phase 3: External Parquet Integration

### Goal
Use existing Parquet files from a data lake as a Kafka topic's data source.

### Use Cases

1. **Data Lake Integration**: Query existing Parquet files via Kafka protocol
2. **Migration**: Import historical data from data lake into Chronik
3. **Hybrid**: Combine external historical data with new streaming data

### Topic Configuration

```bash
# Read-only external Parquet topic
kafka-topics.sh --create --topic historical-events \
  --bootstrap-server localhost:9092 \
  --config storage.mode=external \
  --config external.source=s3 \
  --config external.s3.bucket=data-lake \
  --config external.s3.prefix=events/ \
  --config external.parquet.pattern="year={year}/month={month}/*.parquet" \
  --config external.offset.mode=timestamp \
  --config external.readonly=true

# Hybrid: External read + new writes
kafka-topics.sh --create --topic events \
  --config storage.mode=hybrid \
  --config external.s3.bucket=data-lake \
  --config external.s3.prefix=historical/ \
  --config external.readonly=true \
  --config s3.bucket=streaming-data \
  --config s3.prefix=live/
```

### Implementation Tasks

#### 3.1 External Source Registry
**File:** `crates/chronik-storage/src/external_source.rs` (new)

```rust
/// Registry of external Parquet sources
pub struct ExternalSourceRegistry {
    sources: DashMap<String, ExternalSource>,
}

pub struct ExternalSource {
    pub source_type: ExternalSourceType,
    pub config: ExternalSourceConfig,
    pub schema: Option<Arc<Schema>>,
    pub partition_discovery: PartitionDiscovery,
}

pub enum ExternalSourceType {
    S3Parquet,
    GcsParquet,
    AzureParquet,
    LocalParquet,
    DeltaLake,  // Future: Delta Lake support
    Iceberg,    // Future: Apache Iceberg support
}

#[derive(Clone)]
pub struct ExternalSourceConfig {
    pub bucket: String,
    pub prefix: String,
    pub pattern: String,  // e.g., "partition={partition}/*.parquet"
    pub credentials: Option<CredentialsRef>,
    pub readonly: bool,
    pub offset_mode: OffsetMode,
}

pub enum OffsetMode {
    /// Use row number as offset
    RowNumber,
    /// Use timestamp column as offset
    Timestamp { column: String },
    /// Use explicit offset column
    Column { column: String },
}
```

#### 3.2 Parquet File Discovery
**File:** `crates/chronik-storage/src/parquet_discovery.rs` (new)

```rust
/// Discovers and indexes external Parquet files
pub struct ParquetDiscovery {
    s3_client: Arc<S3Client>,
    index: Arc<RwLock<ParquetFileIndex>>,
}

pub struct ParquetFileIndex {
    /// Map of partition -> sorted list of Parquet files
    files: HashMap<i32, Vec<ParquetFileInfo>>,
    /// Total row count
    total_rows: i64,
    /// Last discovery time
    last_updated: DateTime<Utc>,
}

pub struct ParquetFileInfo {
    pub path: String,
    pub partition: i32,
    pub min_offset: i64,      // First row number / timestamp
    pub max_offset: i64,      // Last row number / timestamp
    pub row_count: i64,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
}

impl ParquetDiscovery {
    /// Scan S3 prefix and build file index
    pub async fn discover(&self, config: &ExternalSourceConfig) -> Result<ParquetFileIndex> {
        let objects = self.s3_client.list_objects(&config.bucket, &config.prefix).await?;

        let mut index = ParquetFileIndex::new();
        for obj in objects {
            if obj.key.ends_with(".parquet") {
                let info = self.get_parquet_info(&obj).await?;
                index.add_file(info);
            }
        }

        Ok(index)
    }

    /// Get Parquet file metadata (row count, schema, min/max values)
    async fn get_parquet_info(&self, obj: &S3Object) -> Result<ParquetFileInfo> {
        // Read Parquet footer to get metadata without downloading entire file
        let footer = self.read_parquet_footer(obj).await?;
        // Extract row count, schema, statistics
    }
}
```

#### 3.3 External Fetch Handler
**File:** `crates/chronik-server/src/external_fetch_handler.rs` (new)

```rust
/// Handles Fetch requests for external Parquet sources
pub struct ExternalFetchHandler {
    discovery: Arc<ParquetDiscovery>,
    reader_cache: Arc<ParquetReaderCache>,
}

impl ExternalFetchHandler {
    /// Fetch records from external Parquet files
    pub async fn fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        // 1. Find Parquet file(s) containing offset
        let files = self.discovery.find_files(topic, partition, offset).await?;

        // 2. Read records from Parquet
        let mut records = Vec::new();
        let mut bytes_read = 0;

        for file in files {
            let reader = self.reader_cache.get_or_open(&file).await?;

            for batch in reader.read_batches(offset, max_bytes - bytes_read)? {
                let kafka_records = self.batch_to_kafka_records(&batch, topic, partition)?;
                bytes_read += kafka_records.iter().map(|r| r.size()).sum::<usize>();
                records.extend(kafka_records);

                if bytes_read >= max_bytes as usize {
                    break;
                }
            }
        }

        Ok(FetchResponse { records })
    }

    /// Convert Arrow RecordBatch to Kafka records
    fn batch_to_kafka_records(
        &self,
        batch: &RecordBatch,
        topic: &str,
        partition: i32,
    ) -> Result<Vec<KafkaRecord>> {
        // Map Parquet columns to Kafka record fields
        // - _key column -> record key
        // - _value column -> record value (or serialize entire row as JSON)
        // - _timestamp column -> record timestamp
        // - Row number -> offset
    }
}
```

#### 3.4 Hybrid Topic Support
**File:** `crates/chronik-server/src/hybrid_handler.rs` (new)

```rust
/// Handles topics with both external (historical) and live data
pub struct HybridHandler {
    external_handler: Arc<ExternalFetchHandler>,
    live_handler: Arc<FetchHandler>,
    boundary_offset: AtomicI64,  // Where external ends and live begins
}

impl HybridHandler {
    /// Fetch from external or live based on offset
    pub async fn fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        let boundary = self.boundary_offset.load(Ordering::Relaxed);

        if offset < boundary {
            // Fetch from external Parquet
            self.external_handler.fetch(topic, partition, offset, max_bytes).await
        } else {
            // Fetch from live WAL/segments
            self.live_handler.fetch(topic, partition, offset, max_bytes).await
        }
    }
}
```

#### 3.5 Schema Mapping
```rust
/// Maps Parquet schema to Kafka record format
pub struct SchemaMapping {
    /// Column to use as Kafka key (optional)
    pub key_column: Option<String>,
    /// Column to use as Kafka value (or serialize all columns)
    pub value_mode: ValueMode,
    /// Column to use as timestamp
    pub timestamp_column: Option<String>,
    /// Column to use as offset (or row number)
    pub offset_column: Option<String>,
}

pub enum ValueMode {
    /// Use specific column as value (binary/string)
    Column(String),
    /// Serialize all columns as JSON
    JsonAll,
    /// Serialize selected columns as JSON
    JsonSelected(Vec<String>),
    /// Use Avro serialization
    Avro { schema_id: i32 },
}
```

### Estimated Effort: 7-10 days

---

## Phase 4: SQL Integration for External Sources

### Goal
Query external Parquet files via SQL API seamlessly.

### Implementation

```sql
-- External Parquet files automatically available as tables
SELECT * FROM historical_events
WHERE timestamp > '2024-01-01'
LIMIT 100;

-- Join external historical with live data
SELECT h.user_id, h.event_type, l.current_status
FROM historical_events h
JOIN live_events l ON h.user_id = l.user_id
WHERE h.timestamp > '2024-01-01';
```

#### 4.1 External Table Registration
**File:** `crates/chronik-server/src/unified_api/sql_handler.rs`

```rust
impl SqlHandler {
    /// Register external Parquet source as SQL table
    async fn register_external_source(
        &self,
        engine: &ColumnarQueryEngine,
        topic: &str,
        config: &ExternalSourceConfig,
    ) -> Result<()> {
        // Use DataFusion's ParquetExec for efficient pushdown
        let table = ListingTable::try_new(ListingTableConfig::new(
            ObjectStorePath::from(&config.prefix),
        ).with_file_format(ParquetFormat::new()))?;

        engine.register_table(topic, Arc::new(table))?;
        Ok(())
    }
}
```

### Estimated Effort: 2-3 days

---

## Summary

| Phase | Feature | Effort | Priority |
|-------|---------|--------|----------|
| 1 | Per-Topic S3 Configuration | 3-4 days | High |
| 2 | Diskless Topics | 5-7 days | Medium |
| 3 | External Parquet Integration | 7-10 days | High |
| 4 | SQL Integration | 2-3 days | Medium |

**Total Estimated Effort: 17-24 days**

## Recommended Implementation Order

1. **Phase 1: Per-Topic S3** (foundation for other features)
2. **Phase 3: External Parquet** (high user value, enables data lake integration)
3. **Phase 2: Diskless Topics** (builds on Phase 1 infrastructure)
4. **Phase 4: SQL Integration** (enhancement to Phase 3)

## Configuration Summary

```bash
# Per-topic S3 (Phase 1)
--config s3.bucket=my-bucket
--config s3.region=us-east-1
--config s3.prefix=data/

# Diskless (Phase 2)
--config storage.mode=diskless
--config diskless.format=parquet
--config diskless.flush.interval.ms=60000

# External Parquet (Phase 3)
--config storage.mode=external
--config external.s3.bucket=data-lake
--config external.s3.prefix=events/
--config external.parquet.pattern="partition={partition}/*.parquet"

# Hybrid (Phase 3)
--config storage.mode=hybrid
--config external.s3.bucket=historical-data
--config s3.bucket=live-data
```

## Open Questions

1. **Credentials Management**: How to securely store per-topic S3 credentials?
   - Option A: Environment variables with topic prefix
   - Option B: Secrets manager integration (AWS Secrets Manager, HashiCorp Vault)
   - Option C: Encrypted storage in metadata WAL

2. **Offset Assignment for External Parquet**: How to assign Kafka offsets to Parquet rows?
   - Option A: Row number (simple but not stable across file changes)
   - Option B: Timestamp-based (requires timestamp column)
   - Option C: Hash-based (deterministic but complex)

3. **Schema Evolution**: How to handle schema changes in external Parquet files?
   - Option A: Strict schema matching (reject incompatible files)
   - Option B: Schema union (merge all schemas)
   - Option C: Schema registry integration

4. **Partition Discovery**: How to map Parquet partitioning to Kafka partitions?
   - Option A: 1:1 mapping (`partition=0/` → Kafka partition 0)
   - Option B: Configurable mapping
   - Option C: Single logical partition (all files → partition 0)
