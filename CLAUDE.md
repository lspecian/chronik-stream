# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chronik Stream is a high-performance Kafka-compatible streaming platform written in Rust that implements the Kafka wire protocol with comprehensive Write-Ahead Log (WAL) durability and automatic recovery. Current version: v2.2.22.

**Key Differentiators:**
- Full Kafka protocol compatibility tested with real clients (kafka-python, confluent-kafka, KSQL, Apache Flink)
- WAL-based metadata store (ChronikMetaLog) for event-sourced metadata persistence
- Zero message loss guarantee through WAL persistence and automatic recovery
- Single unified binary (`chronik-server`) with multiple operational modes
- **Per-topic columnar storage** (Arrow/Parquet) with SQL queries via DataFusion
- **Per-topic vector search** (HNSW + embeddings) for semantic search
- **Unified API on port 6092** for SQL, vector search, admin, and Elasticsearch-compatible endpoints

## Build & Test Commands

### Building
```bash
# Build all components (default: standalone, ingest, search, all modes)
cargo build --release

# Build specific binary
cargo build --release --bin chronik-server

# Check without building
cargo check --all-features --workspace
```

**IMPORTANT**: The chronik-server binary includes both standalone and multi-node Raft cluster modes. All modes (`standalone`, `ingest`, `search`, `all`, `raft-cluster`) are available in a single binary.

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

### Running the Server (v2.2.0+ CLI)

**IMPORTANT**: CLI redesigned in v2.2.0 for simplicity. Old commands removed.

```bash
# Single-node mode (simplest - just works)
cargo run --bin chronik-server start

# Single-node with custom data directory
cargo run --bin chronik-server start --data-dir ./my-data

# Single-node with advertised address (for Docker/remote clients)
cargo run --bin chronik-server start --advertise my-hostname.com:9092

# Cluster mode (from config file)
cargo run --bin chronik-server start --config examples/cluster-3node.toml

# Cluster mode with node ID override
cargo run --bin chronik-server start --config cluster.toml --node-id 2

# Check version and features
cargo run --bin chronik-server version

# Cluster management (Priorities 2-4: All Complete)

# Add node to running cluster (zero-downtime)
cargo run --bin chronik-server cluster add-node 4 --kafka node4:9092 --wal node4:9291 --raft node4:5001 --config cluster.toml

# Query cluster status
cargo run --bin chronik-server cluster status --config cluster.toml

# Remove node from cluster (zero-downtime) - NEW in Priority 4
cargo run --bin chronik-server cluster remove-node 4 --config cluster.toml

# Force remove (for dead/crashed nodes)
cargo run --bin chronik-server cluster remove-node 4 --force --config cluster.toml
```

**Zero-Downtime Node Addition** (Priority 2 - Complete):
- Nodes can be added to a running cluster without downtime
- Automatic partition rebalancing
- Requires authentication via `CHRONIK_ADMIN_API_KEY`
- See [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md) for security setup

**Cluster Status Command** (Priority 3 - Complete):
- Query cluster state: nodes, leader, partitions, ISR
- Pretty-printed output with node roles and partition assignments
- Automatically discovers cluster leader
- Requires authentication via `CHRONIK_ADMIN_API_KEY`

**Zero-Downtime Node Removal** (Priority 4 - Complete):
- Graceful removal: Reassigns partitions away from node before removal
- Force removal: Handles dead/crashed nodes (skips partition reassignment)
- Safety checks: Prevents removing below minimum quorum (3 nodes)
- Automatic re-replication to remaining nodes
- See [docs/TESTING_NODE_REMOVAL.md](docs/TESTING_NODE_REMOVAL.md) for testing guide
- See [docs/PRIORITY4_COMPLETE.md](docs/PRIORITY4_COMPLETE.md) for implementation details

**Auto-detection**: The `start` command automatically detects:
- If `--config` provided or `CHRONIK_CONFIG` set → cluster mode
- If `CHRONIK_CLUSTER_PEERS` env var set → cluster mode
- Otherwise → single-node mode

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

**For development testing - Local Cluster:**

**DEFINITIVE METHOD** - This is THE ONLY way to run a local test cluster:

```bash
# Start 3-node cluster (ports 9092, 9093, 9094)
./tests/cluster/start.sh

# Test with Python Kafka clients
python3 test_node_ready.py

# Stop cluster
./tests/cluster/stop.sh

# View logs
tail -f tests/cluster/logs/node*.log
```

**Location:** `tests/cluster/` contains:
- `node1.toml`, `node2.toml`, `node3.toml` - Node configurations
- `start.sh` - Start cluster script
- `stop.sh` - Stop cluster script
- `logs/` - Log files
- `data/` - Data directories (cleaned on start)

**DO NOT:**
- ❌ Create cluster configs elsewhere
- ❌ Start nodes manually
- ❌ Use Docker on macOS for testing

See [tests/cluster/README.md](tests/cluster/README.md) for details.

## Layered Storage Architecture with Full Disaster Recovery

Chronik implements a 3-tier storage system for infinite retention with automatic disaster recovery (v1.3.65+):

**Tier 1 (Hot - WAL)**: `<1ms` latency, local disk, GroupCommitWal with bincode
**Tier 2 (Warm - Segments)**: `1-10ms` latency, S3/GCS/Azure/local, raw message data
**Tier 3 (Cold - Tantivy)**: `100-500ms` latency, S3/GCS/Azure/local, searchable indexes

### Object Store Configuration

| Backend | Key Environment Variables |
|---------|---------------------------|
| **S3** (default) | `S3_BUCKET`, `S3_REGION`, `S3_ENDPOINT` (for MinIO), `S3_ACCESS_KEY`, `S3_SECRET_KEY` |
| **GCS** | `GCS_BUCKET`, `GCS_PROJECT_ID`, `GCS_PREFIX` |
| **Azure** | `AZURE_ACCOUNT_NAME`, `AZURE_CONTAINER`, `AZURE_USE_EMULATOR` |
| **Local** | `LOCAL_STORAGE_PATH=/data/segments` (default if no backend set) |

### Data Flow

**Write**: Producer → WAL (fsync) → Segments (background) → Tantivy (indexer, every 30s)
**Read**: Try WAL → Try Segments (cached) → Try Tantivy (S3 download) → Metadata fallback
**Metadata DR**: WAL metadata auto-uploads to S3/GCS/Azure every 60s (local: persists on disk)

**Disaster Recovery (v1.3.65+)**: Automatic metadata + data backup to S3/GCS/Azure. Complete cluster recovery from object storage on cold start. See [docs/DISASTER_RECOVERY.md](docs/DISASTER_RECOVERY.md).

**vs. Kafka Tiered Storage**: Chronik Tier 3 provides full-text search on archived data (Tantivy indexes), not just offset-based retrieval. Query old messages by content without scanning.

**Monitoring**: Key metrics: `fetch_*_hit_rate`, `wal_indexer_lag_seconds`. Logs: `RUST_LOG=chronik_server::fetch_handler=debug`

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
- **ChronikMetaLog (WAL-based)** - Event-sourced metadata persistence
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
- `IntegratedKafkaServerBuilder` orchestrates WAL component initialization
- Config flag `use_wal_metadata` controls metadata store type (default: true)

**Builder Pattern for Server Initialization** (v2.2.8+):
The `IntegratedKafkaServer` uses a 14-stage builder pattern to reduce complexity from 764 (30x over threshold) to <25 per function:

```rust
// Single-node mode
let server = IntegratedKafkaServerBuilder::new(config)
    .build()
    .await?;

// Cluster mode with Raft
let server = IntegratedKafkaServerBuilder::new(config)
    .with_raft_cluster(raft_cluster)
    .build()
    .await?;
```

**Builder Stages** (14 stages for separation of concerns):
1. **Directories** - Data directory structure creation
2. **Raft Cluster** - Single-node or multi-node consensus (optional)
3. **Metadata WAL** - Event-sourced metadata persistence
4. **Event Bus** - Metadata replication event channel
5. **Metadata Store** - WalMetadataStore with recovery
6. **Metadata Replication** - Background replication tasks
7. **Storage Layer** - ObjectStore, StorageService, SegmentReader
8. **WAL Manager** - Message WAL with group commit
9. **ProduceHandler** - Message production with ISR tracking
10. **WAL Recovery** - Watermark restoration from WAL
11. **FetchHandler** - Message consumption
12. **KafkaProtocolHandler** - Request routing
13. **WalIndexer** - Background Tantivy indexing
14. **MetadataUploader** - Disaster recovery (S3/GCS/Azure only)

Each stage is independently testable with complexity <25. See [crates/chronik-server/src/integrated_server/builder.rs](crates/chronik-server/src/integrated_server/builder.rs) for implementation.

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

## Operational Modes (v2.2.0+)

**IMPORTANT**: CLI redesigned in v2.2.0. Old subcommands removed.

The `chronik-server` binary now has a single `start` command that auto-detects mode:
- **Single-Node** (default) - Standalone Kafka server with WAL durability
- **Cluster** (from config) - Multi-node cluster with Raft + WAL replication (requires config file)

**Removed in v2.2.0**: `standalone`, `raft-cluster`, `ingest`, `search`, `all` subcommands

### Raft Clustering (Multi-Node Replication)

**CRITICAL**: Raft is ONLY for multi-node clusters (3+ nodes for quorum). Single-node deployments should use standalone mode.

#### Why Single-Node Raft is Rejected

Raft provides **zero benefit** for single-node deployments:
- ❌ No fault tolerance (1 node dies = cluster down)
- ❌ No replication (data only exists on 1 node)
- ❌ Adds latency (Raft state machine overhead)
- ✅ WAL already provides durability for single-node

**Use single-node mode** (v2.2.0+):
```bash
# Correct: Single-node with WAL durability
cargo run --bin chronik-server start
```

#### Multi-Node Cluster Setup (v2.2.0+)

**NEW in v2.2.0**: Cluster configuration via TOML file for clarity and validation.

**Example config files provided**:
- `examples/cluster-3node.toml` - Production template
- `examples/cluster-local-3node.toml` - Local testing template

**Quick Start - 3-Node Local Cluster:**

1. **Create config files** (copy and customize `examples/cluster-local-3node.toml`):
```bash
# Node 1: cluster-node1.toml (set node_id=1, ports: 9092, 9291, 5001)
# Node 2: cluster-node2.toml (set node_id=2, ports: 9093, 9292, 5002)
# Node 3: cluster-node3.toml (set node_id=3, ports: 9094, 9293, 5003)
```

2. **Start nodes**:
```bash
# Build server binary
cargo build --release --bin chronik-server

# Terminal 1 - Node 1
./target/release/chronik-server start --config cluster-node1.toml

# Terminal 2 - Node 2
./target/release/chronik-server start --config cluster-node2.toml

# Terminal 3 - Node 3
./target/release/chronik-server start --config cluster-node3.toml
```

**Config File Format** (see examples/cluster-3node.toml):
```toml
node_id = 1  # CHANGE THIS on each node (1, 2, 3)

replication_factor = 3
min_insync_replicas = 2

[node.addresses]
kafka = "0.0.0.0:9092"    # Where to bind
wal = "0.0.0.0:9291"      # WAL replication receiver
raft = "0.0.0.0:5001"     # Raft consensus

[node.advertise]
kafka = "localhost:9092"   # What clients connect to
wal = "localhost:9291"     # What followers connect to
raft = "localhost:5001"    # What peers connect to

[[peers]]
id = 1
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 2
kafka = "localhost:9093"
wal = "localhost:9292"
raft = "localhost:5002"

[[peers]]
id = 3
kafka = "localhost:9094"
wal = "localhost:9293"
raft = "localhost:5003"
```

**Benefits**:
- ✅ All configuration in one file (reviewable, versionable)
- ✅ Clear separation: bind (where to listen) vs advertise (what clients see)
- ✅ WAL address is explicit (enables automatic replication discovery)
- ✅ No magic port offsets or confusing flags

**Benefits of Raft Cluster:**
- ✅ Quorum-based replication (survives minority failures)
- ✅ Automatic leader election
- ✅ Strong consistency (linearizable reads/writes)
- ✅ Fault tolerance (can lose minority of nodes)
- ✅ Automatic log compaction via snapshots (prevents unbounded log growth)
- ✅ Zero-downtime node addition and removal (v2.2.0+)

### Cluster Management (v2.2.0 - Priorities 2-4 Complete)

**NEW in v2.2.0**: Complete zero-downtime cluster management via CLI and HTTP API.

#### Node Addition (Priority 2)

Add a new node to a running cluster without downtime:

```bash
# Step 1: Add node to cluster membership (run on any existing node)
./target/release/chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config cluster.toml

# Step 2: Start the new node
./target/release/chronik-server start --config cluster-node4.toml

# Step 3: Verify (partitions automatically rebalance)
./target/release/chronik-server cluster status --config cluster.toml
```

**What happens automatically:**
- Node 4 joins Raft cluster
- Partition rebalancer detects new capacity
- Partitions redistribute across all 4 nodes
- WAL replication connects to new node
- Zero client interruptions

#### Node Removal (Priority 4 - NEW!)

Remove a node from a running cluster gracefully:

```bash
# Graceful removal (reassigns partitions first)
./target/release/chronik-server cluster remove-node 4 --config cluster.toml

# What happens automatically:
# 1. Finds all partitions where node 4 is a replica
# 2. Reassigns partitions to other nodes (excluding node 4)
# 3. Waits for new replicas to catch up
# 4. Removes node 4 from Raft cluster
# 5. Node 4 can be safely shut down
```

**Force removal** (for dead/crashed nodes):

```bash
# Skip partition reassignment - use when node is already dead
./target/release/chronik-server cluster remove-node 4 --force --config cluster.toml

# WARNING: Partitions on dead node become under-replicated
# Automatic re-replication will begin to restore replication factor
```

**Safety checks** (prevent invalid operations):
- ✅ Can't remove below minimum 3 nodes (prevents split-brain)
- ✅ Can't remove non-existent node
- ✅ Can't remove self without `--force` (prevents accidental leader removal)
- ✅ Validates quorum will be maintained after removal

#### Cluster Status (Priority 3)

Query cluster state at any time:

```bash
./target/release/chronik-server cluster status --config cluster.toml
```

**Example output:**
```
Chronik Cluster Status
======================

Nodes: 3
Leader: Node 1 (elected by Raft)

Node Details:
  Node 1: kafka=localhost:9092, wal=localhost:9291, raft=localhost:5001
  Node 2: kafka=localhost:9093, wal=localhost:9292, raft=localhost:5002
  Node 3: kafka=localhost:9094, wal=localhost:9293, raft=localhost:5003

Partition Assignments:
  topic1-0: leader=1, replicas=[1, 2, 3], ISR=[1, 2, 3]
  topic1-1: leader=2, replicas=[2, 3, 1], ISR=[2, 3, 1]
  topic2-0: leader=3, replicas=[3, 1, 2], ISR=[3, 1, 2]
```

#### Complete Workflow Example

End-to-end cluster lifecycle:

```bash
# 1. Start 3-node cluster
./target/release/chronik-server start --config cluster-node{1,2,3}.toml

# 2. Check initial status
./target/release/chronik-server cluster status --config cluster-node1.toml
# Shows: 3 nodes

# 3. Add node 4 (zero downtime)
./target/release/chronik-server cluster add-node 4 \
  --kafka localhost:9095 --wal localhost:9294 --raft localhost:5004 \
  --config cluster-node1.toml

# 4. Start node 4
./target/release/chronik-server start --config cluster-node4.toml

# 5. Verify 4 nodes with rebalanced partitions
./target/release/chronik-server cluster status --config cluster-node1.toml
# Shows: 4 nodes, partitions redistributed

# 6. Remove node 4 (zero downtime)
./target/release/chronik-server cluster remove-node 4 --config cluster-node1.toml

# 7. Verify back to 3 nodes
./target/release/chronik-server cluster status --config cluster-node1.toml
# Shows: 3 nodes, partitions rebalanced again

# 8. Stop node 4 (safe - no longer owns partitions)
kill <node4-pid>
```

**Zero downtime achieved:** Clients never interrupted, partitions always available.

#### HTTP Admin API (Alternative Interface)

All cluster commands also available via HTTP API:

```bash
# Get API key from server logs
grep "Admin API key" ./data/node1/chronik.log

# Query status
curl http://localhost:10001/admin/status \
  -H "X-API-Key: <api-key>"

# Add node
curl -X POST http://localhost:10001/admin/add-node \
  -H "X-API-Key: <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"node_id": 4, "kafka_addr": "localhost:9095", "wal_addr": "localhost:9294", "raft_addr": "localhost:5004"}'

# Remove node
curl -X POST http://localhost:10001/admin/remove-node \
  -H "X-API-Key: <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"node_id": 4, "force": false}'
```

**Admin API runs on port:** `10000 + node_id` (e.g., Node 1 → 10001, Node 2 → 10002)

**Security:** All endpoints except `/admin/health` require API key authentication.

See [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md) for details.

### Schema Registry (v2.2.20+)

Chronik includes a Confluent-compatible Schema Registry for managing Avro, JSON Schema, and Protobuf schemas.

**Key Features:**
- Full Confluent Schema Registry REST API compatibility
- Avro, JSON Schema, and Protobuf support
- Schema compatibility checking (BACKWARD, FORWARD, FULL, NONE)
- Optional HTTP Basic Auth (Confluent-compatible)
- Runs on Admin API port (`10000 + node_id`)

#### Quick Start

```bash
# Register a schema
curl -X POST http://localhost:10001/subjects/user-value/versions \
  -H "Content-Type: application/json" \
  -d '{"schema": "{\"type\": \"record\", \"name\": \"User\", \"fields\": [{\"name\": \"id\", \"type\": \"long\"}]}"}'

# List subjects
curl http://localhost:10001/subjects

# Get schema by ID
curl http://localhost:10001/schemas/ids/1
```

#### Authentication

Schema Registry supports optional HTTP Basic Auth:

```bash
# Enable with environment variables
export CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true
export CHRONIK_SCHEMA_REGISTRY_USERS="admin:secret123,readonly:viewonly"

# Usage with curl
curl -u admin:secret123 http://localhost:10001/subjects
```

**See [docs/SCHEMA_REGISTRY.md](docs/SCHEMA_REGISTRY.md) for full documentation.**

#### Testing

Comprehensive test suite for node management:

```bash
# Automated test script
./tests/test_node_removal.sh

# Manual testing guide
# See docs/TESTING_NODE_REMOVAL.md for step-by-step instructions
```

**Implementation details:**
- [docs/PRIORITY4_COMPLETE.md](docs/PRIORITY4_COMPLETE.md) - Node removal design
- [docs/CLUSTER_PLAN_STATUS_UPDATE.md](docs/CLUSTER_PLAN_STATUS_UPDATE.md) - Complete status

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
cargo run --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost \
  --kafka-port 9092 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap

# Disable snapshots for testing
CHRONIK_SNAPSHOT_ENABLED=false cargo run --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost \
  --kafka-port 9092 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap

# Custom snapshot configuration
CHRONIK_SNAPSHOT_LOG_THRESHOLD=50000 \
CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS=7200 \
CHRONIK_SNAPSHOT_COMPRESSION=zstd \
CHRONIK_SNAPSHOT_RETENTION_COUNT=5 \
cargo run --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost \
  --kafka-port 9092 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap
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

## Columnar Storage & SQL Queries (v2.2.22+)

Chronik supports optional per-topic columnar storage using Apache Arrow and Parquet, enabling SQL queries via DataFusion.

### Enabling Columnar Storage

```bash
# Create topic with columnar storage
kafka-topics.sh --create --topic orders \
  --bootstrap-server localhost:9092 \
  --config columnar.enabled=true \
  --config columnar.format=parquet \
  --config columnar.compression=zstd
```

### SQL Query API (Port 6092)

```bash
# Execute SQL query
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT * FROM orders WHERE amount > 50 LIMIT 10"}'

# Explain query plan
curl -X POST http://localhost:6092/_sql/explain \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT COUNT(*) FROM orders"}'

# List tables
curl http://localhost:6092/_sql/tables

# Describe schema
curl http://localhost:6092/_sql/describe/orders
```

### Configuration Options

| Config Key | Values | Default | Description |
|------------|--------|---------|-------------|
| `columnar.enabled` | `true`, `false` | `false` | Enable columnar storage |
| `columnar.format` | `parquet`, `arrow` | `parquet` | Storage format |
| `columnar.compression` | `zstd`, `snappy`, `lz4`, `none` | `zstd` | Compression codec |
| `columnar.partitioning` | `none`, `hourly`, `daily` | `daily` | Time partitioning |

**See [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md) for full documentation.**

## Vector Search & Semantic Queries (v2.2.22+)

Chronik supports optional per-topic vector search using HNSW indexes and embeddings for semantic search.

### Enabling Vector Search

```bash
# Create topic with vector search
kafka-topics.sh --create --topic logs \
  --bootstrap-server localhost:9092 \
  --config vector.enabled=true \
  --config vector.embedding.provider=openai \
  --config vector.embedding.model=text-embedding-3-small \
  --config vector.field=value

# Set OpenAI API key
export OPENAI_API_KEY="sk-..."
```

### Vector Search API (Port 6092)

```bash
# Semantic search by text
curl -X POST http://localhost:6092/_vector/logs/search \
  -H "Content-Type: application/json" \
  -d '{"query": "database connection errors", "k": 10}'

# Hybrid search (vector + full-text)
curl -X POST http://localhost:6092/_vector/logs/hybrid \
  -H "Content-Type: application/json" \
  -d '{"query": "connection timeout", "k": 10, "vector_weight": 0.7}'

# Get index stats
curl http://localhost:6092/_vector/logs/stats

# List vector-enabled topics
curl http://localhost:6092/_vector/topics
```

### Configuration Options

| Config Key | Values | Default | Description |
|------------|--------|---------|-------------|
| `vector.enabled` | `true`, `false` | `false` | Enable vector search |
| `vector.embedding.provider` | `openai`, `external` | `openai` | Embedding provider |
| `vector.embedding.model` | model name | `text-embedding-3-small` | Model to use |
| `vector.field` | `value`, `key`, `$.json.path` | `value` | Field to embed |
| `vector.index.metric` | `cosine`, `euclidean`, `dot` | `cosine` | Distance metric |

**See [docs/VECTOR_SEARCH_GUIDE.md](docs/VECTOR_SEARCH_GUIDE.md) for full documentation.**

## Unified API (Port 6092)

The Unified API provides a single endpoint for all query and admin operations:

```
┌─────────────────────────────────────────────────────────────┐
│                   Unified API (Port 6092)                    │
├─────────────────────────────────────────────────────────────┤
│  /_sql/*           SQL queries (DataFusion)                 │
│  /_vector/*        Vector search (HNSW + embeddings)        │
│  /_search/*        Full-text search (Elasticsearch-compat)  │
│  /admin/*          Cluster management                       │
│  /subjects/*       Schema Registry (Confluent-compatible)   │
│  /health           Health check                             │
└─────────────────────────────────────────────────────────────┘
```

### Key Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/_sql` | POST | Execute SQL query |
| `/_sql/tables` | GET | List queryable topics |
| `/_vector/{topic}/search` | POST | Semantic search |
| `/_vector/{topic}/hybrid` | POST | Hybrid vector + text search |
| `/_search` | POST | Elasticsearch-compatible search |
| `/admin/status` | GET | Cluster status |
| `/subjects` | GET | List Schema Registry subjects |
| `/health` | GET | Health check |

### Background Processing

**All columnar/vector processing happens AFTER produce in the WalIndexer background pipeline:**

```
Producer → WAL (fsync) → Response  (~2-10ms)
               │
               ▼ (background, async)
          WalIndexer
               │
    ┌──────────┼──────────┐
    ▼          ▼          ▼
 Tantivy    Parquet    Embeddings
 (search)   (SQL)      (vector)
```

**Query availability latency**:
- Kafka Fetch: Immediate
- Full-text search: 30-60s
- SQL queries: 30-60s
- Vector search: 30-90s (includes embedding API)

## Environment Variables

Key environment variables:
- `RUST_LOG` - Log level (debug, info, warn, error)
- `CHRONIK_KAFKA_PORT` - Kafka port (default: 9092)
- `CHRONIK_BIND_ADDR` - Bind address (default: 0.0.0.0)
- `CHRONIK_ADVERTISED_ADDR` - **CRITICAL** for Docker/remote access
- `CHRONIK_ADVERTISED_PORT` - Port advertised to clients
- `CHRONIK_DATA_DIR` - Data directory (default: ./data)
- `CHRONIK_PRODUCE_PROFILE` - ProduceHandler flush profile: `low-latency`, `balanced` (default), `high-throughput`
- `CHRONIK_WAL_PROFILE` - WAL commit profile: `low`, `medium`, `high`, `ultra` (auto-detected by default)
- `CHRONIK_WAL_ROTATION_SIZE` - Segment seal threshold: `100KB`, `250MB` (default), `1GB`, or raw bytes `268435456`
- `CHRONIK_ADMIN_API_KEY` - API key for admin API authentication (Priority 2, **REQUIRED for production**)
- `CHRONIK_ADMIN_TLS_CERT` - Path to TLS certificate for admin API (Priority 2, optional)
- `CHRONIK_ADMIN_TLS_KEY` - Path to TLS private key for admin API (Priority 2, optional)
- `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED` - Enable HTTP Basic Auth for Schema Registry (default: `false`)
- `CHRONIK_SCHEMA_REGISTRY_USERS` - Comma-separated `user:pass` pairs for Schema Registry auth
- `CHRONIK_UNIFIED_API_PORT` - Unified API port (default: 6092)
- `OPENAI_API_KEY` - OpenAI API key for vector embeddings
- `CHRONIK_EMBEDDING_PROVIDER` - Default embedding provider: `openai`, `external`
- `CHRONIK_COLUMNAR_FORMAT` - Default columnar format: `parquet`, `arrow`
- `CHRONIK_COLUMNAR_COMPRESSION` - Default compression: `zstd`, `snappy`, `lz4`, `none`
- `CHRONIK_HOT_BUFFER_ENABLED` - Enable hot buffer for sub-second SQL query latency (default: `true`)
- `CHRONIK_HOT_BUFFER_MAX_RECORDS` - Max records per partition in hot buffer (default: `100000`)
- `CHRONIK_HOT_BUFFER_REFRESH_MS` - Hot buffer refresh interval in ms (default: `1000`)
- `CHRONIK_COLUMNAR_USE_OBJECT_STORE` - Enable S3/GCS/Azure for Parquet files (default: `false`, local-first)
- `CHRONIK_COLUMNAR_S3_PREFIX` - Prefix for Parquet files in object storage (default: `columnar`)
- `CHRONIK_COLUMNAR_KEEP_LOCAL` - Keep local copy when uploading to object storage (default: `true`)

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
│   ├── chronik-server/      # Main binary - integrated Kafka server + Unified API
│   ├── chronik-protocol/    # Kafka wire protocol (19 APIs)
│   ├── chronik-wal/         # WAL with recovery, compaction, checkpointing
│   ├── chronik-storage/     # Storage layer, segment I/O, object store, WalIndexer
│   ├── chronik-common/      # Shared types, metadata traits
│   ├── chronik-columnar/    # Arrow/Parquet columnar storage, SQL via DataFusion
│   ├── chronik-embeddings/  # Embedding providers (OpenAI, external)
│   ├── chronik-search/      # Tantivy full-text search integration
│   ├── chronik-query/       # Query processing
│   ├── chronik-monitoring/  # Prometheus metrics, tracing
│   ├── chronik-auth/        # SASL, TLS, ACLs
│   ├── chronik-backup/      # Backup functionality
│   ├── chronik-config/      # Configuration management
│   ├── chronik-cli/         # CLI tools
│   ├── chronik-raft/        # Raft consensus for clustering
│   ├── chronik-raft-bridge/ # Raft integration bridge
│   ├── chronik-benchmarks/  # Performance benchmarks
│   ├── raft/                # Vendored TiKV raft (prost 0.12 compatible)
│   └── raft-proto/          # Vendored raft-proto (prost codegen)
├── tests/integration/       # Integration tests (WAL, Kafka clients, etc.)
├── tests/cluster/           # Local cluster test scripts
├── docs/                    # Documentation (guides, roadmaps)
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
9. **NEVER RELEASE WITHOUT TESTING** - CRITICAL: Do NOT commit, tag, or push releases without actual testing
10. **NEVER CLAIM PRODUCTION-READY WITHOUT TESTING** - Do NOT claim anything is ready, fixed, or production-ready without verification
11. **FIX FORWARD, NEVER REVERT** - CRITICAL: In this house, we NEVER revert commits, delete tags, or rollback releases. We ALWAYS fix forward with the next version. Every bug is an opportunity to learn and improve. Reverting hides problems; fixing forward solves them permanently.

## CRITICAL RULE: NEVER USE GIT CHECKOUT/REVERT ON FILES

**ABSOLUTE PROHIBITION** - This rule was added 2025-11-28 after a catastrophic incident where `git checkout` destroyed 3 days of refactoring work (223 functions, ~3,570 complexity points eliminated).

**FORBIDDEN COMMANDS:**
- ❌ `git checkout <file>` - NEVER use this to revert files
- ❌ `git checkout -- <file>` - NEVER use this pattern
- ❌ `git restore <file>` - NEVER use this to discard changes
- ❌ `git reset --hard` - NEVER use this (destroys all uncommitted work)
- ❌ `git clean -fd` - NEVER use this (deletes untracked files)

**WHAT TO DO INSTEAD:**
1. **Manual Edit** - If you need to undo specific changes, manually edit the file to remove them
2. **Git Stash** - Use `git stash` to temporarily save changes (can be recovered)
3. **Copy First** - Create backups before any destructive operation: `cp file.rs file.rs.backup`
4. **Ask User** - If unsure, ALWAYS ask the user before any file reversion
5. **Edit Tool** - Use the Edit tool to surgically remove unwanted changes, NOT git commands

**TRIPLE CONFIRMATION PROTOCOL:**
IF the user explicitly asks to revert a file using git:
1. **FIRST CONFIRMATION**: "This will permanently destroy all uncommitted changes in <file>. Are you absolutely sure?"
2. **SECOND CONFIRMATION**: "This includes <list specific changes that will be lost>. Confirm you want to lose this work?"
3. **THIRD CONFIRMATION**: "Final warning: No undo possible. Type 'DESTROY CHANGES' to proceed."
ONLY after all 3 confirmations may you proceed.

**INCIDENT REPORT (2025-11-28):**
- **What Happened**: Used `git checkout` to revert 3 files, destroying 3 days of work
- **Files Affected**: produce_handler.rs, wal_replication.rs, metadata_wal_replication.rs
- **Work Lost**: Refactoring Phases 2.1-2.14 modifications to these files
- **Impact**: Had to reconstruct minimal stubs to get build working
- **Lesson**: Git checkout is IRREVERSIBLE for uncommitted changes. There is NO recovery.
- **Emotional Cost**: Developer worked 3 days with minimal sleep. Loss was devastating.

**REMEMBER**: Uncommitted changes destroyed by git are PERMANENTLY LOST. No .git history, no recovery, no undo. NEVER use git to revert files unless absolutely necessary AND triple-confirmed.

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
- **NEVER CHANGE WAL PROFILE WITHOUT ASKING** - The `CHRONIK_WAL_PROFILE` environment variable controls WAL batch intervals (low=2ms, medium=10ms, high=50ms, ultra=100ms). When comparing standalone vs cluster performance, ensure BOTH use the SAME profile. The cluster start script (`tests/cluster/start.sh`) sets `CHRONIK_WAL_PROFILE=high`. Do NOT change it without explicit user permission. If you need to test with a different profile, ASK FIRST.
