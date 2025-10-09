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
- **Standalone** (default) - Single-node Kafka server
- **Ingest** - Data ingestion (future: distributed)
- **Search** - Search node (requires `--features search`)
- **All** - All components in one process

WAL compaction CLI:
```bash
# Manual compaction
chronik-server compact now --strategy key-based

# Show status
chronik-server compact status --detailed

# Configure
chronik-server compact config --enabled true --interval 3600
```

## Environment Variables

Key environment variables:
- `RUST_LOG` - Log level (debug, info, warn, error)
- `CHRONIK_KAFKA_PORT` - Kafka port (default: 9092)
- `CHRONIK_BIND_ADDR` - Bind address (default: 0.0.0.0)
- `CHRONIK_ADVERTISED_ADDR` - **CRITICAL** for Docker/remote access
- `CHRONIK_ADVERTISED_PORT` - Port advertised to clients
- `CHRONIK_DATA_DIR` - Data directory (default: ./data)
- `CHRONIK_FILE_METADATA` - Use file-based metadata (default: false, uses WAL)

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
