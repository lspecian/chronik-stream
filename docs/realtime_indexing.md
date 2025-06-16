# Real-time Indexing Pipeline

The Chronik Stream real-time indexing pipeline provides high-performance indexing of JSON documents with automatic schema detection, Tantivy integration, and seamless integration with the Chronik segment format.

## Features

### 1. Schemaless JSON Support
- **Automatic Field Detection**: Dynamically detects fields in JSON documents without predefined schema
- **Type Inference**: Automatically infers field types (text, numeric, date, boolean)
- **Nested Object Support**: Handles nested JSON structures with configurable depth limits
- **Array Handling**: Properly indexes array fields for efficient searching

### 2. High-Performance Indexing
- **Batching and Buffering**: Optimizes throughput with configurable batch sizes
- **Multi-threaded Processing**: Leverages multiple CPU cores for parallel indexing
- **Memory Management**: Configurable memory budgets prevent OOM issues
- **Backpressure Handling**: Automatic flow control for sustainable throughput

### 3. Tantivy Integration
- **Full-Text Search**: Leverages Tantivy's powerful text analysis capabilities
- **Fast Fields**: Creates fast fields for numeric and keyword data for efficient filtering
- **Real-time Updates**: Supports near real-time search with configurable commit intervals
- **Schema Evolution**: Handles dynamic schema changes without downtime

### 4. Chronik Segment Integration
- **Persistent Storage**: Automatically creates Chronik segments for durable storage
- **Bloom Filters**: Builds bloom filters for efficient key lookups
- **Compression**: Supports multiple compression algorithms (Gzip, Snappy)
- **Combined Format**: Stores both raw data and search indices in a single file

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Kafka Stream   │────>│  JSON Pipeline   │────>│ Realtime Index  │
└─────────────────┘     └──────────────────┘     └─────────────────┘
                               │                          │
                               ▼                          ▼
                        ┌──────────────┐          ┌──────────────┐
                        │ Field Policy │          │   Tantivy    │
                        └──────────────┘          └──────────────┘
                                                          │
                                                          ▼
                                                  ┌──────────────┐
                                                  │Chronik Segment │
                                                  └──────────────┘
```

## Usage

### Basic Configuration

```rust
use chronik_search::{
    RealtimeIndexerConfig, FieldIndexingPolicy,
    JsonPipelineBuilder
};

// Configure field indexing policies
let mut field_policy = FieldIndexingPolicy::default();
field_policy.max_nesting_depth = 3;
field_policy.excluded_fields = vec!["password".to_string()];

// Configure the indexer
let config = RealtimeIndexerConfig {
    index_base_path: PathBuf::from("/data/index"),
    batch_size: 1000,
    batch_timeout: Duration::from_millis(100),
    num_indexing_threads: 4,
    indexing_memory_budget: 512 * 1024 * 1024, // 512MB
    field_policy,
    ..Default::default()
};

// Build the pipeline
let pipeline = JsonPipelineBuilder::new()
    .indexing_memory_budget(config.indexing_memory_budget)
    .batch_size(config.batch_size)
    .build()
    .await?;
```

### Processing Messages

```rust
// Process a batch of Kafka records
let batch = RecordBatch {
    records: vec![
        Record {
            offset: 1,
            timestamp: chrono::Utc::now().timestamp_millis(),
            key: Some(b"user-123".to_vec()),
            value: r#"{"name": "Alice", "age": 30}"#.as_bytes().to_vec(),
            headers: HashMap::new(),
        }
    ],
};

pipeline.process_batch("users", 0, &batch).await?;
```

### Monitoring Performance

```rust
// Get pipeline statistics
let stats = pipeline.stats().await;
println!("Messages processed: {}", stats.messages_processed);
println!("Parse errors: {}", stats.parse_errors);

// Get indexer metrics
let metrics = pipeline.indexer_metrics();
println!("Documents indexed: {}", metrics.documents_indexed);
println!("Memory usage: {} bytes", metrics.memory_usage_bytes);
```

## Configuration Options

### Field Indexing Policy

| Option | Description | Default |
|--------|-------------|---------|
| `index_text` | Enable full-text indexing | `true` |
| `create_fast_fields` | Create fast fields for filtering | `true` |
| `store_values` | Store original field values | `true` |
| `max_nesting_depth` | Maximum JSON nesting depth | `3` |
| `excluded_fields` | Fields to exclude from indexing | `[]` |
| `field_type_hints` | Override automatic type detection | `{}` |

### Indexer Configuration

| Option | Description | Default |
|--------|-------------|---------|
| `batch_size` | Documents per batch | `1000` |
| `batch_timeout` | Maximum time before batch commit | `1s` |
| `num_indexing_threads` | Parallel indexing threads | `4` |
| `indexing_memory_budget` | Memory limit for indexing | `512MB` |
| `target_segment_size` | Target size for segments | `256MB` |
| `compression` | Compression algorithm | `Gzip` |
| `enable_bloom_filters` | Build bloom filters | `true` |

## Performance Characteristics

### Throughput
- Single thread: 10,000-20,000 docs/sec
- Multi-threaded (4 cores): 40,000-80,000 docs/sec
- Depends on document size and complexity

### Latency
- Indexing latency: < 1ms per document
- Search latency: < 10ms for most queries
- Commit interval affects search freshness

### Memory Usage
- Base overhead: ~50MB
- Per-document: ~1-5KB depending on size
- Configurable memory limits prevent OOM

### Storage
- Index size: 10-30% of raw data
- Compression ratio: 50-70% with Gzip
- Bloom filter: ~1 byte per unique key

## Best Practices

1. **Batch Size**: Use larger batches (1000-5000) for throughput, smaller (100-500) for latency
2. **Memory Budget**: Set to 50-75% of available memory
3. **Thread Count**: Set to number of CPU cores minus 1
4. **Field Exclusion**: Exclude large binary or sensitive fields
5. **Nesting Depth**: Limit to 3-4 levels for performance
6. **Monitoring**: Track metrics and adjust configuration based on workload

## Error Handling

The pipeline handles various error scenarios gracefully:

- **Malformed JSON**: Stores as raw text with parse error tracking
- **Schema Conflicts**: Uses most permissive type (e.g., string for mixed types)
- **Memory Pressure**: Applies backpressure and throttling
- **Indexing Failures**: Tracks failed documents without stopping pipeline

## Integration Examples

### With Kafka Consumer

```rust
// Integrate with Kafka consumer
let consumer: StreamConsumer = // ... Kafka setup

while let Some(message) = consumer.recv().await {
    let batch = convert_to_record_batch(message);
    pipeline.process_batch(&topic, partition, &batch).await?;
}
```

### With REST API

```rust
// Expose indexing via HTTP endpoint
async fn index_documents(
    Json(docs): Json<Vec<serde_json::Value>>,
    State(pipeline): State<Arc<JsonPipeline>>,
) -> Result<Json<IndexResponse>> {
    let batch = convert_json_to_batch(docs);
    pipeline.process_batch("api", 0, &batch).await?;
    Ok(Json(IndexResponse { success: true }))
}
```

## Benchmarks

Performance benchmarks on a 4-core machine:

```
Single Document:      50 µs/doc
Batch (100 docs):     30 µs/doc
Batch (1000 docs):    25 µs/doc
Complex JSON:         75 µs/doc
Large JSON (10KB):    200 µs/doc
```

Run benchmarks with:
```bash
cargo bench --bench realtime_indexing_bench
```