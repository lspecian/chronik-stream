# chronik-raft

Raft-based distributed consensus for Chronik Stream partition replication.

## Overview

This crate provides the foundational structure for implementing Raft consensus in Chronik Stream,
enabling high availability and fault tolerance through partition replication across a cluster of nodes.

## Architecture

### Core Components

- **RaftLogStorage**: Trait for persistent storage of Raft log entries
  - `MemoryLogStorage`: In-memory implementation for testing
  - WAL-backed implementation (Phase 2)

- **PartitionReplica**: Manages Raft consensus for a single topic partition
  - Leader election
  - Log replication
  - State machine application

- **RaftService**: gRPC service for inter-node communication
  - AppendEntries RPC
  - RequestVote RPC
  - InstallSnapshot RPC

- **RaftConfig**: Configuration for Raft nodes
  - Node ID and addressing
  - Election and heartbeat timeouts
  - Batch sizes and thresholds

### Module Structure

```
chronik-raft/
├── src/
│   ├── config.rs      # Raft configuration
│   ├── error.rs       # Error types
│   ├── storage.rs     # Log storage trait + implementations
│   ├── replica.rs     # Partition replica management
│   ├── rpc.rs         # gRPC service implementation
│   └── lib.rs         # Public API
├── proto/
│   └── raft_rpc.proto # gRPC service definitions
└── build.rs           # Protobuf code generation
```

## Implementation Status

### Phase 1: Foundational Structure (Current)

- [x] Crate setup with proper dependencies
- [x] Core module structure (config, error, storage, replica, rpc)
- [x] Protobuf definitions for gRPC communication
- [x] RaftLogStorage trait definition
- [x] MemoryLogStorage implementation
- [x] Basic configuration types
- [x] Stub implementations for PartitionReplica and RaftService

### Phase 2: WAL Integration (Planned)

- [ ] WAL-backed RaftLogStorage implementation
- [ ] Mapping Raft entries to WalRecords
- [ ] Recovery from WAL on startup
- [ ] Log compaction via WAL segment rotation

### Phase 3: Full Raft Implementation (Planned)

- [ ] Complete PartitionReplica with raft-rs integration
- [ ] Leader election
- [ ] Log replication
- [ ] Snapshot creation and installation
- [ ] Configuration changes

### Phase 4: Server Integration (Planned)

- [ ] Integrate with chronik-server
- [ ] Partition assignment and rebalancing
- [ ] Metadata coordination
- [ ] Health checks and monitoring

## Design Decisions

### No Circular Dependencies

To avoid circular dependencies with `chronik-wal` and `chronik-storage`:

- Phase 1 uses only `chronik-common` as a dependency
- `MemoryLogStorage` for testing without WAL integration
- WAL dependencies will be added via optional features in Phase 2

### Protobuf for RPC

Using protobuf with tonic provides:
- Type-safe RPC definitions
- Forward/backward compatibility
- Language-agnostic protocol for future clients

### Trait-based Storage

`RaftLogStorage` trait enables:
- Testing with in-memory storage
- Production with WAL-backed storage
- Future storage backends without API changes

## Dependencies

- `raft` (0.7): TiKV's Raft implementation with `prost-codec` feature
  - **Note**: Uses Prost (pure Rust) instead of rust-protobuf
  - **No protoc required**: Build works on systems without Protocol Buffer compiler
- `tonic` (0.12): gRPC framework
- `prost` (0.13): Protobuf serialization
- Standard Chronik dependencies (serde, tokio, async-trait, etc.)

### Build Requirements

**Simplified** (no protoc needed):
1. Rust 1.75+ (workspace edition 2021)
2. Standard Cargo build: `cargo build -p chronik-raft`

**Previous Requirement Removed**: Protocol Buffers Compiler (`protoc`) is no longer needed thanks to `prost-codec` feature.

## Testing

```bash
# Run unit tests
cargo test -p chronik-raft

# Run with logging
RUST_LOG=debug cargo test -p chronik-raft -- --nocapture

# Check compilation
cargo check -p chronik-raft
```

## Example (Phase 1)

```rust
use chronik_raft::{RaftConfig, PartitionReplica, MemoryLogStorage, RaftEntry};

// Create configuration
let config = RaftConfig {
    node_id: 1,
    listen_addr: "0.0.0.0:5001".to_string(),
    ..Default::default()
};

// Create partition replica (stub implementation)
let replica = PartitionReplica::new(
    "my-topic".to_string(),
    0,
    config,
)?;

// Check partition info
assert_eq!(replica.info(), ("my-topic", 0));
```

## Future Work

See the [Raft Clustering Roadmap](../../docs/RAFT_CLUSTERING_ROADMAP.md) for detailed implementation plans.

## License

Apache-2.0
