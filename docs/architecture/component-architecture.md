# Component Architecture

This document provides detailed information about each component in Chronik Stream, their internal architecture, and how they interact with each other.

## Broker Architecture

### Overview

The broker is the core component that handles client requests, manages data storage, and coordinates distributed operations.

```
┌─────────────────────────────────────────────────┐
│                    Broker                        │
│                                                 │
│  ┌───────────────────────────────────────────┐ │
│  │          Network Layer                     │ │
│  │  ┌─────────┐  ┌─────────┐  ┌───────────┐ │ │
│  │  │Acceptor │  │Protocol │  │Connection │ │ │
│  │  │Thread   │  │Decoder  │  │Pool       │ │ │
│  │  └─────────┘  └─────────┘  └───────────┘ │ │
│  └───────────────────────────────────────────┘ │
│                                                 │
│  ┌───────────────────────────────────────────┐ │
│  │          Request Processing                │ │
│  │  ┌─────────┐  ┌─────────┐  ┌───────────┐ │ │
│  │  │Request  │  │Auth     │  │Rate       │ │ │
│  │  │Router   │  │Handler  │  │Limiter    │ │ │
│  │  └─────────┘  └─────────┘  └───────────┘ │ │
│  └───────────────────────────────────────────┘ │
│                                                 │
│  ┌───────────────────────────────────────────┐ │
│  │          Core Services                     │ │
│  │  ┌─────────┐  ┌─────────┐  ┌───────────┐ │ │
│  │  │Log      │  │Replica  │  │Group      │ │ │
│  │  │Manager  │  │Manager  │  │Coordinator│ │ │
│  │  └─────────┘  └─────────┘  └───────────┘ │ │
│  └───────────────────────────────────────────┘ │
│                                                 │
│  ┌───────────────────────────────────────────┐ │
│  │          Storage & Index                   │ │
│  │  ┌─────────┐  ┌─────────┐  ┌───────────┐ │ │
│  │  │Segment  │  │Index    │  │Cache      │ │ │
│  │  │Store    │  │Builder  │  │Manager    │ │ │
│  │  └─────────┘  └─────────┘  └───────────┘ │ │
│  └───────────────────────────────────────────┘ │
└─────────────────────────────────────────────────┘
```

### Network Layer

#### Acceptor Thread
```rust
pub struct Acceptor {
    listener: TcpListener,
    connection_manager: Arc<ConnectionManager>,
    tls_config: Option<TlsConfig>,
}

impl Acceptor {
    async fn accept_loop(&self) {
        loop {
            let (socket, addr) = self.listener.accept().await?;
            let conn = Connection::new(socket, addr);
            self.connection_manager.add_connection(conn).await;
        }
    }
}
```

#### Protocol Decoder
Handles Kafka protocol parsing:
- Request header parsing
- API version negotiation
- Request body deserialization
- Response serialization

#### Connection Pool
- Maintains active client connections
- Connection lifecycle management
- Idle connection cleanup
- Per-connection state tracking

### Request Processing

#### Request Router
Routes requests to appropriate handlers based on API key:

```rust
pub enum ApiKey {
    Produce = 0,
    Fetch = 1,
    ListOffsets = 2,
    Metadata = 3,
    // ... more API keys
}

pub async fn route_request(request: Request) -> Response {
    match request.api_key {
        ApiKey::Produce => handle_produce(request).await,
        ApiKey::Fetch => handle_fetch(request).await,
        ApiKey::Metadata => handle_metadata(request).await,
        // ... more handlers
    }
}
```

#### Authentication Handler
- SASL authentication flow
- Certificate validation for mTLS
- Token validation for OAuth
- Session management

#### Rate Limiter
- Token bucket algorithm
- Per-client quotas
- Configurable limits
- Quota violation handling

### Core Services

#### Log Manager
Manages the commit log for each partition:

```rust
pub struct LogManager {
    logs: HashMap<TopicPartition, Log>,
    config: LogConfig,
    scheduler: CleanupScheduler,
}

pub struct Log {
    segments: Vec<Segment>,
    active_segment: Segment,
    index: OffsetIndex,
    time_index: TimeIndex,
}
```

Features:
- Segment rotation
- Log compaction
- Retention management
- Recovery from crashes

#### Replica Manager
Handles replication between brokers:

```rust
pub struct ReplicaManager {
    replicas: HashMap<TopicPartition, Replica>,
    fetcher_threads: Vec<FetcherThread>,
    leader_epoch_cache: LeaderEpochCache,
}

impl ReplicaManager {
    async fn replicate(&self, partition: TopicPartition) {
        let replica = self.replicas.get(&partition);
        if replica.is_leader() {
            self.handle_produce_request(partition).await;
        } else {
            self.fetch_from_leader(partition).await;
        }
    }
}
```

#### Group Coordinator
Manages consumer groups:

```rust
pub struct GroupCoordinator {
    groups: HashMap<String, ConsumerGroup>,
    rebalance_manager: RebalanceManager,
    offset_manager: OffsetManager,
}

pub struct ConsumerGroup {
    id: String,
    members: HashMap<String, GroupMember>,
    state: GroupState,
    assignments: HashMap<String, Vec<TopicPartition>>,
}
```

## Storage Engine

### Architecture

```
Storage Engine
├── Segment Management
│   ├── Active Segment Writer
│   ├── Segment Rotation
│   └── Segment Cleanup
├── Index Management
│   ├── Offset Index
│   ├── Timestamp Index
│   └── Search Index
└── Cache Layer
    ├── Page Cache
    ├── Index Cache
    └── Metadata Cache
```

### Segment Structure

```rust
pub struct Segment {
    base_offset: i64,
    log_file: File,
    index_file: OffsetIndex,
    time_index: TimeIndex,
    size: u64,
    created_time: i64,
}

impl Segment {
    pub async fn append(&mut self, batch: RecordBatch) -> Result<i64> {
        // Validate batch
        self.validate_batch(&batch)?;
        
        // Write to log
        let position = self.log_file.seek(SeekFrom::End(0))?;
        self.log_file.write_all(&batch.to_bytes())?;
        
        // Update indices
        self.index_file.append(batch.last_offset, position)?;
        self.time_index.append(batch.max_timestamp, batch.last_offset)?;
        
        // Update size
        self.size += batch.size_in_bytes();
        
        Ok(batch.last_offset)
    }
}
```

### Index Types

#### Offset Index
Maps offset to file position:
```
Offset -> Position
1000   -> 0
2000   -> 104857
3000   -> 209714
```

#### Timestamp Index
Maps timestamp to offset:
```
Timestamp     -> Offset
1704801234567 -> 1000
1704801235678 -> 2000
1704801236789 -> 3000
```

#### Search Index
Tantivy-based inverted index for full-text search.

## Search Engine Integration

### Architecture

```
Search Engine
├── Index Pipeline
│   ├── Document Extractor
│   ├── Field Analyzer
│   └── Index Writer
├── Query Engine
│   ├── Query Parser
│   ├── Query Planner
│   └── Result Merger
└── Index Management
    ├── Segment Merger
    ├── Index Optimizer
    └── Cache Warmer
```

### Index Pipeline

```rust
pub struct IndexPipeline {
    extractors: Vec<Box<dyn DocumentExtractor>>,
    analyzers: HashMap<String, Box<dyn Analyzer>>,
    writer: IndexWriter,
}

impl IndexPipeline {
    pub async fn index_batch(&self, records: Vec<Record>) {
        let documents = self.extract_documents(records).await;
        let analyzed = self.analyze_documents(documents).await;
        self.writer.add_documents(analyzed).await?;
        self.writer.commit().await?;
    }
}
```

### Query Processing

```rust
pub struct QueryEngine {
    searcher: IndexSearcher,
    parser: QueryParser,
    executor: QueryExecutor,
}

impl QueryEngine {
    pub async fn search(&self, query: SearchQuery) -> SearchResults {
        // Parse query
        let parsed = self.parser.parse(&query.query_string)?;
        
        // Plan execution
        let plan = self.create_execution_plan(parsed, &query.partitions);
        
        // Execute across partitions
        let partial_results = self.executor.execute_parallel(plan).await;
        
        // Merge results
        self.merge_results(partial_results)
    }
}
```

## Metadata Management
With a topic WAL

### Metadata Types

```rust
pub struct TopicMetadata {
    pub name: String,
    pub partitions: Vec<PartitionMetadata>,
    pub configs: HashMap<String, String>,
    pub creation_time: i64,
}

pub struct PartitionMetadata {
    pub id: i32,
    pub leader: i32,
    pub replicas: Vec<i32>,
    pub isr: Vec<i32>,
    pub offline_replicas: Vec<i32>,
}

pub struct BrokerMetadata {
    pub id: i32,
    pub host: String,
    pub port: i32,
    pub rack: Option<String>,
    pub status: BrokerStatus,
}
```

## Consumer Group Coordination

### State Machine

```
         ┌─────────┐
         │  Empty  │
         └────┬────┘
              │ First member joins
              ▼
      ┌───────────────┐
      │ PreparingRebalance │◄────────┐
      └───────┬───────┘              │
              │ All members joined    │ Rebalance triggered
              ▼                      │
         ┌─────────┐                 │
         │ Stable  │─────────────────┘
         └────┬────┘
              │ All members leave
              ▼
         ┌─────────┐
         │  Dead   │
         └─────────┘
```

### Rebalance Protocol

```rust
pub struct RebalanceManager {
    protocol_type: String,
    protocols: HashMap<String, AssignmentStrategy>,
}

impl RebalanceManager {
    pub async fn perform_rebalance(&self, group: &mut ConsumerGroup) {
        // Phase 1: Prepare
        group.prepare_rebalance().await;
        
        // Phase 2: Join
        let member_metadata = self.collect_member_metadata(&group).await;
        
        // Phase 3: Sync
        let assignments = self.compute_assignment(member_metadata).await;
        
        // Phase 4: Distribute
        self.distribute_assignments(group, assignments).await;
        
        // Phase 5: Stabilize
        group.transition_to_stable().await;
    }
}
```

## Performance Optimizations

### Zero-Copy Transfer

```rust
pub async fn send_file_zero_copy(
    file: &File,
    socket: &TcpStream,
    offset: u64,
    length: u64,
) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut offset = offset as i64;
        let result = unsafe {
            libc::sendfile(
                socket.as_raw_fd(),
                file.as_raw_fd(),
                &mut offset,
                length as usize,
            )
        };
        // Handle result
    }
    
    #[cfg(not(target_os = "linux"))]
    {
        // Fallback implementation
    }
}
```

### Memory Management

```rust
pub struct MemoryPool {
    pools: Vec<Pool>,
    allocator: JemallocAllocator,
}

pub struct Pool {
    size_class: usize,
    free_list: Vec<Box<[u8]>>,
    allocated: AtomicUsize,
}

impl MemoryPool {
    pub fn allocate(&self, size: usize) -> Box<[u8]> {
        let size_class = self.get_size_class(size);
        let pool = &self.pools[size_class];
        
        if let Some(buffer) = pool.free_list.pop() {
            buffer
        } else {
            self.allocator.allocate(size_class)
        }
    }
}
```

## Monitoring & Metrics

### Metrics Collection

```rust
pub struct MetricsCollector {
    registry: Registry,
    exporters: Vec<Box<dyn MetricExporter>>,
}

impl MetricsCollector {
    pub fn record_request(&self, api_key: ApiKey, duration: Duration) {
        self.registry
            .histogram("broker_request_duration_seconds")
            .with_label("api", api_key.name())
            .observe(duration.as_secs_f64());
    }
}
```

### Health Checks

```rust
pub struct HealthChecker {
    checks: Vec<Box<dyn HealthCheck>>,
}

#[async_trait]
pub trait HealthCheck {
    async fn check(&self) -> HealthStatus;
    fn name(&self) -> &str;
}

pub struct StorageHealthCheck;

#[async_trait]
impl HealthCheck for StorageHealthCheck {
    async fn check(&self) -> HealthStatus {
        // Check disk space
        // Check I/O latency
        // Check segment integrity
    }
}
```

## Next Steps

- Understand [Data Flow](data-flow.md) through components
- Learn about [Storage Architecture](storage-architecture.md) in detail
- Explore [Search Architecture](search-architecture.md)
- Review [Replication & Fault Tolerance](replication.md)