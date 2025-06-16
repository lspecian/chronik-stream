# Produce Request Handler

The Produce Request Handler is a core component of Chronik Stream that handles Kafka produce requests with full protocol compatibility while providing advanced features like real-time search indexing, idempotent producers, and transactional support.

## Features

### Core Functionality
- **Full Kafka Protocol Compatibility**: Supports Kafka produce API versions 0-9
- **Acknowledgment Modes**: 
  - `acks=0`: Fire and forget (no acknowledgment)
  - `acks=1`: Wait for leader acknowledgment
  - `acks=-1/all`: Wait for all in-sync replicas
- **Compression Support**: Gzip, Snappy, LZ4, and Zstd
- **Batch Processing**: Efficient handling of multiple records per request

### Advanced Features
- **Idempotent Producer Support**: 
  - Deduplication based on producer ID and sequence numbers
  - Protection against duplicate messages
  - Out-of-order sequence detection
- **Transactional Support**: 
  - Transaction coordination
  - Atomic writes across partitions
  - Exactly-once semantics
- **Real-time Search Integration**:
  - Automatic indexing with Tantivy
  - JSON document parsing and field extraction
  - Immediate searchability of produced messages

### Storage and Performance
- **Chronik Segment Integration**: 
  - Efficient segment-based storage
  - Bloom filters for fast key lookups
  - Compression for storage efficiency
- **Object Storage Backend**: 
  - S3, GCS, Azure Blob Storage support
  - Local filesystem for development
- **Memory Management**:
  - Configurable buffer limits
  - Back-pressure handling
  - Automatic segment rotation

## Architecture

```
┌─────────────────┐
│ Kafka Producer  │
└────────┬────────┘
         │ Produce Request
         ▼
┌─────────────────────────────────────┐
│      Produce Request Handler        │
├─────────────────────────────────────┤
│ • Protocol Parsing                  │
│ • Topic/Partition Validation        │
│ • Leader Check                      │
│ • Idempotence Validation           │
└────────┬────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│     Record Processing Pipeline      │
├─────────────────────────────────────┤
│ • Offset Assignment                 │
│ • Timestamp Generation              │
│ • Compression                       │
│ • Buffer Management                 │
└────┬────────────────────┬───────────┘
     │                    │
     ▼                    ▼
┌─────────────┐    ┌──────────────────┐
│   Storage   │    │ Search Indexing  │
├─────────────┤    ├──────────────────┤
│ • Segments  │    │ • JSON Parsing   │
│ • Object    │    │ • Field Extract  │
│   Store     │    │ • Tantivy Index  │
└─────────────┘    └──────────────────┘
```

## Configuration

```rust
pub struct ProduceHandlerConfig {
    /// Node ID for this broker
    pub node_id: i32,
    
    /// Enable real-time search indexing
    pub enable_indexing: bool,
    
    /// Enable idempotent producer support
    pub enable_idempotence: bool,
    
    /// Enable transactional producer support
    pub enable_transactions: bool,
    
    /// Maximum in-flight requests per connection
    pub max_in_flight_requests: usize,
    
    /// Batch size for buffering records
    pub batch_size: usize,
    
    /// Time to wait before flushing (ms)
    pub linger_ms: u64,
    
    /// Compression type for storage
    pub compression_type: CompressionType,
    
    /// Request timeout (ms)
    pub request_timeout_ms: u64,
    
    /// Buffer memory limit (bytes)
    pub buffer_memory: usize,
}
```

## Usage Example

```rust
use chronik_ingest::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use chronik_storage::object_store::backends::s3::S3ObjectStore;
use chronik_common::metadata::traits::MetadataStore;

// Create storage backend
let storage = Arc::new(S3ObjectStore::new(/* config */)?);

// Create metadata store
let metadata_store = Arc::new(/* your metadata store */);

// Configure handler
let config = ProduceHandlerConfig {
    node_id: 1,
    enable_indexing: true,
    enable_idempotence: true,
    compression_type: CompressionType::Gzip,
    ..Default::default()
};

// Create handler
let handler = ProduceHandler::new(config, storage, metadata_store).await?;

// Handle produce requests
let response = handler.handle_produce(request, correlation_id).await?;
```

## Performance Optimizations

1. **Batching**: Records are batched in memory before writing to storage
2. **Compression**: Configurable compression reduces storage and network overhead
3. **Async I/O**: Non-blocking operations for storage and indexing
4. **Memory Limits**: Configurable memory limits prevent OOM conditions
5. **Segment Rotation**: Automatic rotation based on size and time thresholds

## Error Handling

The handler provides detailed error codes compatible with Kafka protocol:
- `UNKNOWN_TOPIC_OR_PARTITION`: Topic or partition doesn't exist
- `NOT_LEADER_FOR_PARTITION`: This broker is not the leader
- `DUPLICATE_SEQUENCE_NUMBER`: Duplicate message detected (idempotence)
- `OUT_OF_ORDER_SEQUENCE_NUMBER`: Sequence number gap detected
- `INVALID_PRODUCER_EPOCH`: Producer epoch mismatch
- `INVALID_TXN_STATE`: Invalid transaction state

## Metrics

The handler tracks various metrics for monitoring:
- `records_produced`: Total records successfully produced
- `bytes_produced`: Total bytes written
- `produce_errors`: Number of failed produce requests
- `duplicate_records`: Duplicate records detected (idempotence)
- `segments_created`: Number of segments created
- `indexing_lag_ms`: Indexing pipeline latency
- `compression_ratio`: Compression effectiveness

## Testing

Comprehensive test coverage includes:
- Basic produce functionality
- Idempotent producer scenarios
- Multiple partition handling
- Compression testing
- Different acknowledgment modes
- Error conditions
- Metrics validation

Run tests with:
```bash
cargo test -p chronik-ingest produce_handler
```

## Integration Points

1. **Metadata Store**: Topic and partition metadata management
2. **Object Storage**: Segment persistence
3. **Search Engine**: Real-time indexing with Tantivy
4. **Replication**: ISR management for acks=-1
5. **Consumer Groups**: Offset tracking for consumption