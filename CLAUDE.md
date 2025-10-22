# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chronik Stream is a high-performance Kafka-compatible streaming platform written in Rust that implements the Kafka wire protocol with comprehensive Write-Ahead Log (WAL) durability and automatic recovery. Current version: v1.3.11.

**Key Differentiators:**
- Full Kafka protocol compatibility tested with real clients (kafka-python, confluent-kafka, KSQL, Apache Flink)
- WAL-based metadata store (ChronikMetaLog) for event-sourced metadata persistence
- Zero message loss guarantee through WAL persistence and automatic recovery
- Single unified binary (`chronik-server`) with multiple operational modes

## Build & Test Commands

### Building
```bash
# Build all components
cargo build --release

# Build specific binary
cargo build --release --bin chronik-server

# Check without building
cargo check --all-features --workspace
```

### Testing
```bash
# Run unit tests (lib and bins only - skip integration)
cargo test --workspace --lib --bins

# Run specific test
cargo test --test <test_name>

# Run test with output
cargo test <test_name> -- --nocapture

# Run integration tests (requires Docker for some)
cargo test --test integration

# Run benchmarks
cargo bench
```

### Running the Server
```bash
# Development mode (default standalone)
cargo run --bin chronik-server

# Standalone with options
cargo run --bin chronik-server -- standalone --dual-storage

# With advertised address (required for Docker/remote clients)
cargo run --bin chronik-server -- --advertised-addr localhost standalone

# All components mode
cargo run --bin chronik-server -- all

# Check version and features
cargo run --bin chronik-server -- version
```

### Docker

**IMPORTANT: DO NOT use Docker for testing during development on macOS.**
- Docker Desktop on macOS is slow and resource-intensive
- Native binary testing is much faster and more reliable
- Use Docker only for CI/CD or final integration testing, not for iterative development

```bash
# Build Docker image (CI/CD only)
docker build -t chronik-stream .

# Run with Docker (CI/CD only)
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  chronik-stream

# Using docker-compose (CI/CD only)
docker-compose up -d
```

**For development testing on macOS:**
```bash
# Test with native binary instead
./target/release/chronik-server --advertised-addr localhost standalone --dual-storage

# Test with Python Kafka clients
python3 test_script.py

# Test with kafka-console-producer/consumer (if installed via brew)
kafka-console-producer --bootstrap-server localhost:9092 --topic test
```

## Layered Storage Architecture with Full Disaster Recovery

**CRITICAL**: Chronik implements a unique 3-tier layered storage system that stores BOTH raw message data AND search indexes in object storage (S3/GCS/Azure). **NEW in v1.3.65**: Metadata (topics, partitions, offsets) is also backed up to object storage for complete disaster recovery.

### The 3 Tiers (Always Active by Default)

```
┌─────────────────────────────────────────────────────────────────┐
│              Chronik 3-Tier Seamless Storage                     │
│                   (Infinite Retention Design)                    │
├─────────────────────────────────────────────────────────────────┤
│  Tier 1: WAL (Hot - Local Disk)                                 │
│  ├─ Location: ./data/wal/{topic}/{partition}/wal_{p}_{id}.log   │
│  ├─ Data: GroupCommitWal (bincode CanonicalRecords)             │
│  ├─ Latency: <1ms (in-memory buffer)                            │
│  └─ Retention: Until sealed (256MB or 30min threshold)          │
│        ↓ Background WalIndexer (every 30s)                       │
│                                                                   │
│  Tier 2: Raw Segments in S3 (Warm - Object Storage)             │
│  ├─ Location: s3://.../segments/{topic}/{partition}/{min}-{max} │
│  ├─ Data: Bincode Vec<CanonicalRecord> (with wire bytes)        │
│  ├─ Latency: 50-200ms (S3 download + LRU cache)                 │
│  ├─ Retention: Unlimited (cheap object storage)                 │
│  └─ Purpose: Message consumption after local WAL deletion        │
│        ↓ PLUS ↓                                                  │
│                                                                   │
│  Tier 3: Tantivy Indexes in S3 (Cold - Searchable)              │
│  ├─ Location: s3://.../indexes/{topic}/partition-{p}/segment... │
│  ├─ Data: Compressed tar.gz Tantivy search indexes              │
│  ├─ Latency: 100-500ms (S3 + decompress + search)               │
│  ├─ Retention: Unlimited                                         │
│  └─ Purpose: Full-text search, timestamp range queries          │
│                                                                   │
│  Consumer Fetch Flow (Automatic Fallback):                      │
│    Phase 1: Try WAL buffer (hot, in-memory) → μs latency        │
│    Phase 2 MISS: Download raw segment from S3 → cache → serve   │
│    Phase 3 MISS: Search Tantivy index → fetch → serve           │
│                                                                   │
│  Local Disk Cleanup:                                             │
│    - WAL files DELETED after upload to S3 (both raw + index)    │
│    - Tier 2 uses SegmentCache (LRU) to avoid repeated downloads │
│    - Old messages still accessible from S3 indefinitely          │
└─────────────────────────────────────────────────────────────────┘
```

### Object Store Configuration (Tier 3)

Configure where Tantivy archives are stored via environment variables:

**S3-Compatible (MinIO, Wasabi, DigitalOcean Spaces, etc.)**:
```bash
OBJECT_STORE_BACKEND=s3
S3_ENDPOINT=http://minio:9000           # Custom endpoint (MinIO, etc.)
S3_REGION=us-east-1                     # AWS region
S3_BUCKET=chronik-storage               # Bucket name
S3_ACCESS_KEY=minioadmin                # Access key
S3_SECRET_KEY=minioadmin                # Secret key
S3_PATH_STYLE=true                      # Required for MinIO (path-style URLs)
S3_DISABLE_SSL=false                    # Set true for local HTTP MinIO
S3_PREFIX=chronik/                      # Optional prefix for all keys
```

**Local Filesystem** (default if no env vars set):
```bash
OBJECT_STORE_BACKEND=local
LOCAL_STORAGE_PATH=/data/segments       # Path for Tantivy archives
```

**Google Cloud Storage**:
```bash
OBJECT_STORE_BACKEND=gcs
GCS_BUCKET=chronik-storage              # GCS bucket name
GCS_PROJECT_ID=my-project               # GCP project ID
GCS_PREFIX=chronik/                     # Optional prefix
```

**Azure Blob Storage**:
```bash
OBJECT_STORE_BACKEND=azure
AZURE_ACCOUNT_NAME=myaccount            # Storage account name
AZURE_CONTAINER=chronik-storage         # Container name
AZURE_USE_EMULATOR=false                # Use Azurite emulator for local dev
```

### How Layered Storage Works

**Write Path** (Automatic):
```
Producer → WAL (Tier 1, immediate) → Segments (Tier 2, seconds) → Tantivy Archives (Tier 3, minutes)
            ↓                           ↓                              ↓
        Durable fsync          Background flush              WalIndexer background thread
                                                             (uploads to object store)
```

**Read Path** (3-Phase Fetch with Automatic Fallback):
```
Consumer Request
    ↓
Phase 1: Try WAL buffer (hot, in-memory)
    ↓ MISS
Phase 2: Try segment files (warm, local disk)
    ↓ MISS
Phase 3: Try Tantivy archives (cold, object store - S3/GCS/Azure/Local)
    ↓ MISS
Fallback: Reconstruct from metadata
```

**Performance Characteristics**:
- **Tier 1 (WAL)**: < 1ms latency, seconds retention
- **Tier 2 (Segments)**: 1-10ms latency, minutes-hours retention
- **Tier 3 (Tantivy)**: 100-500ms latency, unlimited retention (configurable)

### Disaster Recovery (v1.3.65+)

**CRITICAL NEW FEATURE**: Chronik now backs up **both data AND metadata** to remote object storage, enabling complete disaster recovery.

**How It Works:**
- **Local filesystem (default)**: Metadata already persists locally to `./data/wal/__meta/` and survives restarts. **No additional backup needed.**
- **S3/GCS/Azure**: Metadata uploader **automatically activates** to copy metadata to remote storage every 60s. Provides protection against complete node loss.

**Object Store Backends:**
- ✅ **S3** (AWS, MinIO, Wasabi, etc.) - Automatic metadata DR
- ✅ **Google Cloud Storage** - Automatic metadata DR
- ✅ **Azure Blob Storage** - Automatic metadata DR
- ✅ **Local filesystem** (default) - Metadata persists locally, survives restarts

**What Gets Backed Up:**
1. **Message Data** (Already in v1.3.64):
   - Tier 2: Raw segments → `s3://.../segments/`
   - Tier 3: Tantivy indexes → `s3://.../indexes/`

2. **Metadata** (NEW in v1.3.65):
   - Metadata WAL → `s3://.../metadata-wal/`
   - Metadata snapshots → `s3://.../metadata-snapshots/`
   - Includes: topics, partitions, high watermarks, consumer offsets, consumer groups

**Automatic Recovery on Cold Start (with S3):**
```bash
# Scenario: Complete node loss (EC2 terminated, disk gone)

# Step 1: Provision new node with same S3 bucket
export OBJECT_STORE_BACKEND=s3
export S3_BUCKET=chronik-prod  # Same bucket as before
export S3_REGION=us-west-2

# Step 2: Start Chronik (automatic recovery happens)
cargo run --bin chronik-server -- standalone

# What happens automatically:
# 1. Detects empty local WAL
# 2. Downloads latest metadata snapshot from S3
# 3. Downloads metadata WAL segments from S3
# 4. Replays events to restore:
#    - ✅ All topics and partitions
#    - ✅ High watermarks
#    - ✅ Consumer offsets
#    - ✅ Consumer group state
# 5. Server starts normally

# Step 3: Verify (everything should be restored)
kafka-topics --list --bootstrap-server localhost:9092
kafka-console-consumer --topic my-topic --from-beginning --bootstrap-server localhost:9092
```

**Configuration:**
```bash
# Enable metadata DR (default: enabled)
CHRONIK_METADATA_DR=true  # Already enabled by default

# Change upload interval (default: 60s)
CHRONIK_METADATA_UPLOAD_INTERVAL=120 cargo run --bin chronik-server

# Disable if using file-based metadata
CHRONIK_FILE_METADATA=true cargo run --bin chronik-server  # DR disabled
```

**See [docs/DISASTER_RECOVERY.md](docs/DISASTER_RECOVERY.md) for complete DR guide.**

### Key Differentiators vs Kafka Tiered Storage

| Feature | Kafka Tiered Storage | Chronik Layered Storage |
|---------|---------------------|-------------------------|
| **Hot Storage** | Local disk | WAL + Segments (local) |
| **Cold Storage** | S3 (raw data) | Tantivy archives (S3/GCS/Azure) |
| **Auto-archival** | ✅ Yes | ✅ Yes (WalIndexer) |
| **Query by Offset** | ✅ Yes | ✅ Yes |
| **Full-text Search** | ❌ NO | ✅ **YES** (Tantivy) |
| **Query by Content** | ❌ NO | ✅ **YES** (search API) |
| **Compression** | Minimal | High (tar.gz archives) |
| **Read Archived Data** | Slow (S3 fetch) | Fast (indexed search) |

**Unique Advantage**: Chronik's Tier 3 isn't just "cold storage" - it's a **searchable indexed archive**. You can query old data by content without scanning!

### Data Lifecycle Example

```bash
# Minute 0: Producer sends message
# → Immediately written to WAL (Tier 1)
# → Available for consumption in < 1ms

# Minute 1: Background flush
# → Data moved to local segments (Tier 2)
# → Still available for consumption in ~5ms

# Minute 2: WalIndexer runs (every 30s by default)
# → Data indexed by Tantivy
# → Compressed tar.gz uploaded to S3/GCS/Azure
# → Now in Tier 3 (searchable archive)
# → Consumption from archive: ~200ms (includes S3 download + decompress + search)

# Days later: Data still accessible
# → Consumer requests old offset
# → Phase 3 fetch: Download from S3, search index, return results
# → Full-text search also available via Search API
```

### Configuration Examples

**Example 1: MinIO for Development**
```bash
# Docker Compose setup
OBJECT_STORE_BACKEND=s3
S3_ENDPOINT=http://minio:9000
S3_BUCKET=chronik-dev
S3_ACCESS_KEY=minioadmin
S3_SECRET_KEY=minioadmin
S3_PATH_STYLE=true
S3_DISABLE_SSL=true  # Local HTTP MinIO

cargo run --bin chronik-server -- standalone
```

**Example 2: AWS S3 for Production**
```bash
# Production with real S3
OBJECT_STORE_BACKEND=s3
S3_REGION=us-west-2
S3_BUCKET=chronik-prod-archives
# Credentials from IAM role or ~/.aws/credentials

cargo run --bin chronik-server -- standalone
```

**Example 3: Default Local Storage**
```bash
# No env vars = local filesystem for all tiers
cargo run --bin chronik-server -- standalone
# Tantivy archives stored in ./data/segments/
```

### Monitoring Layered Storage

**Key Metrics**:
- `fetch_wal_hit_rate` - % served from Tier 1 (should be high for recent data)
- `fetch_segment_hit_rate` - % served from Tier 2
- `fetch_tantivy_hit_rate` - % served from Tier 3
- `wal_indexer_lag_seconds` - How far behind WalIndexer is
- `tantivy_archive_size_bytes` - Total size in object store

**Logs to Watch**:
```bash
# Check which tier is serving fetches
RUST_LOG=chronik_server::fetch_handler=debug cargo run --bin chronik-server

# Look for:
# - "Serving from WAL buffer" (Tier 1)
# - "Serving from local segment" (Tier 2)
# - "Fetching from Tantivy archive" (Tier 3)
```

### Troubleshooting

**Issue**: Tantivy archives not uploading to S3
- Check `OBJECT_STORE_BACKEND` is set correctly
- Verify S3 credentials and bucket access
- Check logs: `RUST_LOG=chronik_storage::wal_indexer=debug`

**Issue**: Slow Tier 3 fetches (> 1 second)
- Check network latency to S3/GCS/Azure
- Consider increasing `wal_indexing_interval_secs` for larger segments
- Use local caching (WalIndexer supports local cache)

**Issue**: Running out of local disk space
- Tier 2 segments are automatically cleaned up after indexing
- Configure retention: `segment_writer_config.retention_period_secs`
- Tier 3 archives can be stored on unlimited object storage

## Architecture Overview

### Core Components

**1. Kafka Protocol Layer** (`chronik-protocol`)
- Implements Kafka wire protocol (19 APIs fully supported)
- Frame-based codec with length-prefixed messages
- Request/response handling for Produce (v0-v9), Fetch (v0-v13), Metadata, Consumer Groups, etc.
- Protocol version negotiation and compatibility
- Location: `crates/chronik-protocol/src/`

**2. Write-Ahead Log (WAL)** (`chronik-wal`)
- **Mandatory for all deployments** - provides zero message loss guarantee
- Segments with rotation, compaction, and checkpointing
- Automatic recovery on startup from WAL records
- Truncation of old segments after persistence
- Location: `crates/chronik-wal/src/`
- Key modules:
  - `manager.rs` - WalManager for lifecycle
  - `segment.rs` - Segment handling
  - `compaction.rs` - WalCompactor with strategies (key-based, time-based, hybrid)
  - `checkpoint.rs` - CheckpointManager

**3. Metadata Store** (`chronik-common/metadata`)
- Trait-based abstraction (`MetadataStore`)
- Two implementations:
  - **ChronikMetaLog (WAL-based)** - Default, event-sourced metadata
  - File-based (legacy) - Use `--file-metadata` flag
- Stores topics, partitions, consumer groups, offsets
- Location: `crates/chronik-common/src/metadata/`

**4. Storage Layer** (`chronik-storage`)
- Segment-based storage with writers and readers
- Object store abstraction (local, S3, GCS, Azure)
- `SegmentWriter` - Buffered writes with batching
- `SegmentReader` - Efficient fetch with caching
- Optional indexing for search (Tantivy integration)
- Location: `crates/chronik-storage/src/`

**5. Server** (`chronik-server`)
- Unified binary supporting multiple modes
- Integrated Kafka server with request routing
- Handlers for Produce, Fetch, Consumer Groups
- Main entry: `crates/chronik-server/src/main.rs`
- Protocol handler: `crates/chronik-server/src/kafka_handler.rs`

### Data Flow

1. **Write Path (Produce)**:
   - Kafka client → Protocol handler → WalProduceHandler (WAL write) → ProduceHandler → SegmentWriter → Object Store
   - WAL record written FIRST for durability, then in-memory + disk

2. **Read Path (Fetch)**:
   - Kafka client → Protocol handler → FetchHandler → SegmentReader → Object Store
   - Metadata consulted for partition offsets and segment locations

3. **Recovery Path**:
   - Server startup → WalManager.recover() → Replay WAL records → Restore in-memory state
   - Automatic and transparent to clients

### Key Architecture Patterns

**Request Routing** (`crates/chronik-server/src/kafka_handler.rs`):
```rust
// Parse API key from request header
let header = parse_request_header(&mut buf)?;
match ApiKey::try_from(header.api_key)? {
    ApiKey::Produce => self.wal_handler.handle_produce(...),
    ApiKey::Fetch => self.fetch_handler.handle_fetch(...),
    ApiKey::Metadata => self.protocol_handler.handle_metadata(...),
    // ... consumer group APIs, etc.
}
```

**WAL Integration** (MANDATORY):
- `WalProduceHandler` wraps `ProduceHandler`
- All produce requests write to WAL before in-memory state
- `IntegratedKafkaServer::new()` always creates WAL components
- Config flag `use_wal_metadata` controls metadata store type (default: true)

**Metadata Store Abstraction**:
```rust
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_topic(&self, topic: &str, config: TopicConfig) -> Result<TopicMetadata>;
    async fn get_topic(&self, topic: &str) -> Result<Option<TopicMetadata>>;
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>>;
    async fn get_partition_count(&self, topic: &str) -> Result<i32>;
    async fn get_high_watermark(&self, topic: &str, partition: i32) -> Result<i64>;
    async fn set_high_watermark(&self, topic: &str, partition: i32, offset: i64) -> Result<()>;
    // ... consumer group methods
}
```

## Testing Strategy

### Integration Tests
Location: `tests/integration/`

**Key Test Files:**
- `kafka_compatibility_test.rs` - Real Kafka client testing
- `wal_recovery_test.rs` - Crash recovery scenarios
- `consumer_groups.rs` - Consumer group functionality
- `kafka_protocol_test.rs` - Protocol conformance

**Running Integration Tests:**
```bash
# WAL recovery tests
cargo test --test wal_recovery_test

# Kafka compatibility (may need Docker)
cargo test --test kafka_compatibility_test

# Specific test
cargo test test_basic_produce_fetch -- --nocapture
```

### Protocol Conformance Tests
Location: `crates/chronik-protocol/tests/`

Tests wire format compliance, version negotiation, error handling

### Testing with Real Clients

**Python (kafka-python):**
```python
from kafka import KafkaProducer, KafkaConsumer

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0)  # Specify version
)
producer.send('test-topic', b'Hello Chronik!')
```

**KSQL Integration:**
Full KSQL compatibility - see `docs/KSQL_INTEGRATION_GUIDE.md`

## Critical Implementation Details

### Advertised Address Configuration
**CRITICAL**: When binding to `0.0.0.0` or running in Docker, `CHRONIK_ADVERTISED_ADDR` MUST be set, or clients will receive `0.0.0.0:9092` and fail to connect.

```bash
# Correct
CHRONIK_ADVERTISED_ADDR=localhost cargo run --bin chronik-server

# Docker
docker run -e CHRONIK_ADVERTISED_ADDR=chronik-stream chronik-stream
```

### WAL Recovery Flow
1. Server starts → `WalManager::recover(config)`
2. Scans WAL directory for segments
3. Replays records in order to restore state
4. Sets high watermarks for partitions
5. Server ready to accept requests

After successful persistence, old WAL segments are truncated.

### Protocol Version Handling
- `ApiVersions` (v0-v3) - Negotiate supported versions
- Version-specific encoding/decoding in `chronik-protocol`
- Flexible tag field support for newer protocol versions
- Must handle kafka-python's v0 compatibility requirements

### Consumer Group Coordination
- `GroupManager` tracks groups and members
- `FindCoordinator` returns this node as coordinator
- `JoinGroup` / `SyncGroup` / `Heartbeat` / `LeaveGroup` fully implemented
- Offset commit/fetch stored in metadata store

## Common Development Tasks

### Adding a New Kafka API
1. Define request/response types in `chronik-protocol/src/` (follow existing patterns like `create_topics_types.rs`)
2. Add ApiKey enum variant
3. Implement handler in `chronik-server/src/` or `chronik-protocol/src/handler.rs`
4. Add routing in `kafka_handler.rs`
5. Write protocol conformance test in `chronik-protocol/tests/`
6. Test with real Kafka client

### Modifying WAL Behavior
1. Update `WalConfig` in `chronik-wal/src/config.rs`
2. Modify `WalManager` or `WalSegment` logic
3. **CRITICAL**: Ensure recovery logic (`WalManager::recover()`) handles changes
4. Add integration test in `tests/integration/wal_recovery_test.rs`
5. Test crash scenarios

### Storage Backend Changes
1. Implement `ObjectStoreTrait` for new backend
2. Add to `ObjectStoreFactory` in `chronik-storage/src/object_store.rs`
3. Update `ObjectStoreConfig` enum
4. Test with `storage_test.rs`

## Operational Modes

The `chronik-server` binary supports:
- **Standalone** (default) - Single-node Kafka server with WAL-only (no Raft)
- **Raft Cluster** - Multi-node Kafka cluster with Raft consensus (requires `--raft` flag and 3+ nodes)
- **Ingest** - Data ingestion (future: distributed)
- **Search** - Search node (requires `--features search`)
- **All** - All components in one process

### Raft Clustering (Multi-Node Replication)

**CRITICAL**: Raft is ONLY for multi-node clusters (3+ nodes for quorum). Single-node deployments should use standalone mode.

#### Why Single-Node Raft is Rejected

Raft provides **zero benefit** for single-node deployments:
- ❌ No fault tolerance (1 node dies = cluster down)
- ❌ No replication (data only exists on 1 node)
- ❌ Adds latency (Raft state machine overhead)
- ✅ WAL already provides durability for single-node

**Use standalone mode for single-node:**
```bash
# Correct: Single-node with WAL-only (fast path)
cargo run --bin chronik-server -- standalone

# REJECTED: Single-node with Raft (will error)
cargo run --bin chronik-server -- --raft standalone  # Error: "Raft requires 3+ nodes"
```

#### Multi-Node Raft Cluster Setup

**Minimum 3 nodes required for quorum:**
```bash
# Node 1 (port 9092)
cargo run --bin chronik-server -- \
  --advertised-addr node1:9092 \
  --raft \
  standalone

# Node 2 (port 9093)
cargo run --bin chronik-server -- \
  --advertised-addr node2:9092 \
  --raft \
  standalone

# Node 3 (port 9094)
cargo run --bin chronik-server -- \
  --advertised-addr node3:9092 \
  --raft \
  standalone
```

**Raft Configuration:**
- `chronik-cluster.toml` defines cluster topology
- Each node gets a unique `node_id` (1, 2, 3, etc.)
- Raft gRPC port = Kafka port + 100 (e.g., 9192 for node on 9092)
- Requires peer connectivity on both Kafka and Raft ports

**Benefits of Raft Cluster:**
- ✅ Quorum-based replication (survives minority failures)
- ✅ Automatic leader election
- ✅ Strong consistency (linearizable reads/writes)
- ✅ Fault tolerance (can lose minority of nodes)
- ✅ Automatic log compaction via snapshots (prevents unbounded log growth)

### Raft Snapshots & Log Compaction (v2.0.0+)

**NEW**: Chronik Raft clusters now support automatic snapshot-based log compaction to prevent unbounded Raft log growth.

#### How Snapshots Work

```
┌─────────────────────────────────────────────────────────────┐
│                   Raft Snapshot Lifecycle                   │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  1. AUTOMATIC TRIGGER (background loop every 5 minutes)    │
│     ├─ Log size threshold (default: 10,000 entries)        │
│     └─ Time threshold (default: 1 hour)                    │
│                                                             │
│  2. SNAPSHOT CREATION                                       │
│     ├─ Serialize partition state (metadata + data)         │
│     ├─ Compress with Gzip/Zstd (default: Gzip)            │
│     ├─ Upload to S3/GCS/Azure/Local object storage        │
│     └─ CRC32 checksum for integrity                        │
│                                                             │
│  3. LOG TRUNCATION                                          │
│     ├─ Truncate Raft log up to snapshot index             │
│     └─ Free disk space                                     │
│                                                             │
│  4. NODE RECOVERY (on startup)                             │
│     ├─ Download latest snapshot from object storage       │
│     ├─ Apply to Raft state machine                        │
│     └─ Replay only log entries AFTER snapshot             │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

#### Configuration

Configure snapshots via environment variables:

```bash
# Enable/disable (default: true)
CHRONIK_SNAPSHOT_ENABLED=true

# Create snapshot after N log entries (default: 10,000)
CHRONIK_SNAPSHOT_LOG_THRESHOLD=10000

# Create snapshot after N seconds (default: 3600 = 1 hour)
CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS=3600

# Maximum concurrent snapshots (default: 2)
CHRONIK_SNAPSHOT_MAX_CONCURRENT=2

# Compression: gzip (default), none, zstd
CHRONIK_SNAPSHOT_COMPRESSION=gzip

# Keep last N snapshots (default: 3)
CHRONIK_SNAPSHOT_RETENTION_COUNT=3
```

**Snapshot Path Format**:
```
s3://{bucket}/{prefix}/snapshots/{topic}/{partition}/{snapshot_id}.snap
```

#### Performance Characteristics

**Snapshot Creation**:
- 10,000 entries: ~1-2 seconds (Gzip compression)
- 100,000 entries: ~10-20 seconds
- Size reduction: ~70-80% with Gzip

**Node Recovery**:
- Snapshot bootstrap: ~10 seconds for 50MB
- Full log replay: ~30-60 seconds for 100K entries
- **Speedup**: 3-6x faster recovery

**Disk Space Savings**:
- Before compaction: ~100MB per 10K entries
- After snapshot: ~5MB compressed
- **Savings**: ~95% disk space

#### Example Usage

```bash
# Start Raft cluster with snapshots enabled (default)
cargo run --features raft --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost:9092 \
  standalone --raft

# Disable snapshots for testing
CHRONIK_SNAPSHOT_ENABLED=false cargo run --features raft --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost:9092 \
  standalone --raft

# Custom snapshot configuration
CHRONIK_SNAPSHOT_LOG_THRESHOLD=50000 \
CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS=7200 \
CHRONIK_SNAPSHOT_COMPRESSION=zstd \
CHRONIK_SNAPSHOT_RETENTION_COUNT=5 \
cargo run --features raft --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost:9092 \
  standalone --raft
```

#### Monitoring

Check snapshot status via logs:
```bash
# Snapshot creation logs
grep "Creating snapshot for" /var/log/chronik/server.log

# Snapshot upload logs
grep "Snapshot uploaded to" /var/log/chronik/server.log

# Log truncation logs
grep "Truncated Raft log" /var/log/chronik/server.log
```

WAL compaction CLI:
```bash
# Manual compaction
chronik-server compact now --strategy key-based

# Show status
chronik-server compact status --detailed

# Configure
chronik-server compact config --enabled true --interval 3600
```

## Performance Tuning

### ProduceHandler Flush Profiles (v1.3.56+)

The ProduceHandler has configurable flush profiles that control when buffered messages become visible to consumers. Choose the profile that matches your workload:

#### Available Profiles

**1. Low-Latency** (`CHRONIK_PRODUCE_PROFILE=low-latency`)
- **Use case**: Real-time analytics, instant messaging, live dashboards
- **Settings**: 1 batch / 10ms flush
- **Expected latency**: < 20ms p99
- **Buffer memory**: 16MB
- **Trade-off**: More CPU usage for minimal latency

**2. Balanced** (default)
- **Use case**: General-purpose streaming, typical microservices
- **Settings**: 10 batches / 100ms flush
- **Expected latency**: 100-150ms p99
- **Buffer memory**: 32MB
- **Trade-off**: Good balance of throughput and latency

**3. High-Throughput** (`CHRONIK_PRODUCE_PROFILE=high-throughput`)
- **Use case**: Log aggregation, data pipelines, ETL, batch processing
- **Settings**: 100 batches / 500ms flush
- **Expected latency**: < 500ms p99
- **Buffer memory**: 128MB
- **Trade-off**: Maximum throughput at cost of higher latency

#### Usage Examples

```bash
# Low-latency for real-time applications
CHRONIK_PRODUCE_PROFILE=low-latency ./target/release/chronik-server --advertised-addr localhost standalone

# High-throughput for bulk ingestion
CHRONIK_PRODUCE_PROFILE=high-throughput ./target/release/chronik-server --advertised-addr localhost standalone

# Balanced (default, no env var needed)
./target/release/chronik-server --advertised-addr localhost standalone
```

#### Benchmarking Profiles

Test all profiles with comprehensive workloads:
```bash
# Build server first
cargo build --release --bin chronik-server

# Run profile benchmark (tests all 3 profiles with 1K/10K/50K/100K messages)
python3 tests/test_produce_profiles.py
```

The benchmark measures:
- Produce rate (msg/s)
- Consume rate (msg/s)
- Success rate (%)
- End-to-end latency (p50, p95, p99)

### WAL Performance Profiles

See GroupCommitWal configuration in `chronik-wal/src/group_commit.rs` for WAL-level tuning via `CHRONIK_WAL_PROFILE`.

## Environment Variables

Key environment variables:
- `RUST_LOG` - Log level (debug, info, warn, error)
- `CHRONIK_KAFKA_PORT` - Kafka port (default: 9092)
- `CHRONIK_BIND_ADDR` - Bind address (default: 0.0.0.0)
- `CHRONIK_ADVERTISED_ADDR` - **CRITICAL** for Docker/remote access
- `CHRONIK_ADVERTISED_PORT` - Port advertised to clients
- `CHRONIK_DATA_DIR` - Data directory (default: ./data)
- `CHRONIK_FILE_METADATA` - Use file-based metadata (default: false, uses WAL)
- `CHRONIK_PRODUCE_PROFILE` - ProduceHandler flush profile: `low-latency`, `balanced` (default), `high-throughput`
- `CHRONIK_WAL_PROFILE` - WAL commit profile: `low`, `medium`, `high`, `ultra` (auto-detected by default)
- `CHRONIK_WAL_ROTATION_SIZE` - Segment seal threshold: `100KB`, `250MB` (default), `1GB`, or raw bytes `268435456`

## CRC and Checksum Architecture

**CRITICAL Understanding** (to prevent future debugging loops):

Chronik has **THREE SEPARATE CRC/CHECKSUM SYSTEMS** for different purposes:

### 1. Kafka RecordBatch CRC-32C (Wire Protocol)
**Location**: Inside each RecordBatch sent by producers
**Purpose**: Validate batch integrity from producer to consumer
**Algorithm**: CRC-32C (Castagnoli polynomial)
**Calculated Over**: partition_leader_epoch through end of batch (position 12+)
**Preservation Strategy**:
- `CanonicalRecord.compressed_records_wire_bytes` stores ORIGINAL compressed bytes
- Compression (gzip/snappy/zstd) is NON-DETERMINISTIC (timestamps, OS flags)
- Re-compressing produces DIFFERENT bytes → DIFFERENT CRC → Java clients reject
- **Solution**: Preserve original wire bytes, never re-compress (v1.3.18, v1.3.28-v1.3.59)

### 2. WalRecord V2 CRC32 (Internal WAL Integrity)
**Location**: WalRecord V2 header (offset 0x08-0x0B)
**Purpose**: Detect corruption in WAL files
**Algorithm**: CRC32 (IEEE 802.3)
**Problem**: Bincode serialization + preserved wire bytes = non-deterministic
**Solution (v1.3.63)**: **SKIP** checksum validation for V2 records entirely
- V2 records already have Kafka's CRC-32C for data integrity
- Adding second CRC on bincode-serialized data is redundant and error-prone
- V1 records still get full checksum validation (no Kafka CRC)

**Why V2 Checksum Fails**:
```rust
// At write time:
let record = WalRecord::new_v2(...);  // Calculates CRC over struct fields
record.to_bytes();  // Serializes with that CRC

// At read time:
let record = WalRecord::from_bytes(data);  // Deserializes struct
record.verify_checksum();  // Recalculates CRC → DIFFERENT VALUE!
// Because: canonical_data contains compressed_records_wire_bytes which are
// non-deterministic, plus bincode serialization variations
```

**The Fix (v1.3.63)**:
```rust
pub fn verify_checksum(&self) -> Result<()> {
    if self.is_v2() {
        return Ok(());  // Skip - rely on Kafka's CRC-32C
    }
    // V1 records still validated normally
}
```

### 3. Segment File Checksum (Chronik Segment Format)
**Location**: SegmentHeader.checksum (offset 0x22-0x25)
**Purpose**: Validate entire .segment file integrity
**Algorithm**: CRC32 (IEEE 802.3)
**Calculated Over**: Entire file from magic to end
**Used For**: Tier 2 raw segments uploaded to S3

**Historical CRC Issues** (Learn from these!):
- **v1.3.28**: Switched from CRC-32 to CRC-32C for Kafka protocol compatibility
- **v1.3.29**: Fixed CRC-32C endianness and byte range bugs
- **v1.3.30**: Removed incorrect 4-byte padding from RecordBatch
- **v1.3.31-v1.3.32**: Fixed CRC-32C bugs for Java client compatibility
- **v1.3.33**: Fixed CRC corruption by filtering batches in segment fetch
- **v1.3.59**: **MAJOR**: Preserve CRC when updating base_offset during produce
- **v1.3.63**: **MAJOR**: Skip WalRecord V2 checksum validation (redundant with Kafka CRC)

**Key Lesson**: Don't add redundant CRC layers! Each adds complexity and failure modes.

## Debugging Tips

### Protocol Debugging
```bash
# Enable debug logging for protocol
RUST_LOG=chronik_protocol=debug,chronik_server=debug cargo run --bin chronik-server
```

### WAL Debugging
```bash
# WAL-specific logging
RUST_LOG=chronik_wal=debug cargo run --bin chronik-server

# Check WAL recovery
RUST_LOG=chronik_wal::manager=trace cargo run --bin chronik-server
```

### Client Connection Issues
1. Check advertised address is set correctly
2. Verify client can resolve hostname
3. Check firewall/network rules
4. Enable protocol tracing: `RUST_LOG=chronik_protocol::frame=trace`

## CI/CD

GitHub Actions workflows (`.github/workflows/`):
- **CI** (`ci.yml`) - Check, test (lib/bins only), build on PRs
- **Release** (`release.yml`) - Multi-arch builds, Docker images, GitHub releases

Runs on self-hosted runner. Tests skip integration by default (`--lib --bins`).

## Project Structure Summary

```
chronik-stream/
├── crates/
│   ├── chronik-server/      # Main binary - integrated Kafka server
│   ├── chronik-protocol/    # Kafka wire protocol (19 APIs)
│   ├── chronik-wal/          # WAL with recovery, compaction, checkpointing
│   ├── chronik-storage/     # Storage layer, segment I/O, object store
│   ├── chronik-common/      # Shared types, metadata traits
│   ├── chronik-search/      # Optional Tantivy search integration
│   ├── chronik-query/       # Query processing
│   ├── chronik-monitoring/  # Prometheus metrics, tracing
│   ├── chronik-auth/        # SASL, TLS, ACLs
│   ├── chronik-backup/      # Backup functionality
│   ├── chronik-config/      # Configuration management
│   ├── chronik-cli/         # CLI tools
│   └── chronik-benchmarks/  # Performance benchmarks
├── tests/integration/       # Integration tests (WAL, Kafka clients, etc.)
└── .github/workflows/       # CI/CD pipelines
```

## Work Ethic and Quality Standards

**Core Principles:**
1. **NO SHORTCUTS** - Always implement proper, production-ready solutions
2. **CLEAN CODE** - Maintain clean codebase without experimental debris
3. **OPERATIONAL EXCELLENCE** - Focus on reliability, performance, maintainability
4. **COMPLETE SOLUTIONS** - Finish what you start, test thoroughly, document properly
5. **ARCHITECTURAL INTEGRITY** - One implementation, not multiple partial ones
6. **PROFESSIONAL STANDARDS** - Write code as if going to production tomorrow
7. **ABSOLUTE OWNERSHIP** - NEVER say "beyond scope", "not my responsibility", or "separate issue". If you discover a problem while working, YOU OWN IT. Fix it completely.
8. **END-TO-END VERIFICATION** - A fix is NOT complete until verified with real client testing (produce → crash → recover → consume). No excuses.
7. **NEVER RELEASE WITHOUT TESTING** - CRITICAL: Do NOT commit, tag, or push releases without actual testing
8. **NEVER CLAIM PRODUCTION-READY WITHOUT TESTING** - Do NOT claim anything is ready, fixed, or production-ready without verification
9. **FIX FORWARD, NEVER REVERT** - CRITICAL: In this house, we NEVER revert commits, delete tags, or rollback releases. We ALWAYS fix forward with the next version. Every bug is an opportunity to learn and improve. Reverting hides problems; fixing forward solves them permanently.

**Development Process:**
1. Understand existing code fully before changes
2. Plan properly - comprehensive plans, not quick fixes
3. Implement correctly - use the right approach even if it takes longer
4. **TEST FIRST, RELEASE SECOND** - ALWAYS test changes with real clients BEFORE committing/tagging
5. Clean as you go - no experimental files or dead code
6. Document decisions - explain architectural choices and trade-offs

**Quality Metrics:**
- Code should be production-ready on first implementation
- All features MUST be tested with real Kafka clients BEFORE release
- Error handling must be comprehensive
- Performance considered in every decision
- Security and reliability are non-negotiable

**Testing Requirements:**
- **CRITICAL**: On macOS development, NEVER use Docker for testing Chronik
- Docker builds take too long on macOS and Docker networking doesn't work well on macOS
- Always test Chronik natively with `cargo run --bin chronik-server`
- **CRITICAL**: Test with THE ACTUAL CLIENT that reported the bug
  - If Java clients report issues, test with Java clients (KSQLDB, kafka-console-consumer, Java producer/consumer)
  - If Python clients report issues, test with Python clients (kafka-python)
  - Testing with a different client than the one that failed is NOT testing
- **CRITICAL**: Test the EXACT failure scenario reported by the user
  - If user reports CRC errors, verify CRC validation passes
  - If user reports connection failures, verify connections succeed
  - Reproducing success with a different scenario is NOT testing the fix
- Java client testing location: `ksql/confluent-7.5.0/` contains KSQLDB and Java Kafka libraries
- Test BEFORE committing, tagging, or pushing any release
- **DO THINGS PROPERLY** - There is no point in doing things halfway or incorrectly. Properly is the normal, it's how you do things.
