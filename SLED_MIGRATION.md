# Sled Metadata Store Migration

This document describes the migration from PostgreSQL to Sled for metadata storage in Chronik Stream.

## What Was Changed

### 1. Created MetadataStore Trait (`crates/chronik-common/src/metadata/traits.rs`)

The `MetadataStore` trait provides an abstract interface for metadata operations:
- Topic CRUD operations
- Segment metadata management
- Broker registration and status
- Partition assignments
- Consumer group management
- Consumer offset tracking

### 2. Implemented Sled-based Storage (`crates/chronik-common/src/metadata/sled_store.rs`)

- Uses Sled embedded database for persistent storage
- Organized data in separate trees: topics, segments, brokers, assignments, groups, offsets
- Namespaced key format: `topic/{name}`, `segment/{topic}/{id}`, etc.
- Binary serialization using bincode for efficiency

### 3. Updated Controller Node

- Added `metadata_path` configuration
- Initializes Sled metadata store on startup
- Removed PostgreSQL dependency
- Metadata persisted to volume mount in Docker

### 4. Key Design Decisions

- **Subtrees**: Used Sled trees to separate different metadata types
- **Key Format**: Hierarchical key structure for efficient prefix scans
- **Cascading Deletes**: Topic deletion automatically removes associated segments
- **System State**: Automatic creation of internal topics (`__consumer_offsets`, `__transaction_state`)

## Usage

### Basic Example

```rust
use chronik_common::metadata::{MetadataStore, SledMetadataStore, TopicConfig};

// Initialize store
let store = SledMetadataStore::new("/var/chronik/metadata")?;
store.init_system_state().await?;

// Create topic
let config = TopicConfig {
    partition_count: 6,
    replication_factor: 3,
    ..Default::default()
};
let topic = store.create_topic("events", config).await?;
```

### Docker Configuration

The controller now uses a volume for metadata persistence:

```yaml
controller:
  environment:
    METADATA_PATH: /home/chronik/metadata
  volumes:
    - controller_metadata:/home/chronik/metadata
```

## Benefits

1. **No External Dependencies**: Sled is embedded, no separate database process
2. **Better Performance**: Direct memory-mapped I/O, no network overhead
3. **Simpler Operations**: No migrations, schema management, or connection pooling
4. **Rust Native**: Pure Rust implementation, better integration
5. **Future Extensibility**: Easy to add caching layer or alternative backends

## Testing

Comprehensive tests are included in `crates/chronik-common/src/metadata/tests.rs`:
- System initialization
- Topic CRUD operations
- Segment management
- Broker operations
- Partition assignments
- Consumer groups and offsets
- Cascading deletes

Run tests with:
```bash
cargo test -p chronik-common metadata
```

## Migration Path

For existing deployments using PostgreSQL:

1. Export data from PostgreSQL tables
2. Use the metadata store API to recreate topics, brokers, etc.
3. Update configuration to use Sled paths
4. Remove PostgreSQL dependencies

See `examples/migrate_to_sled.rs` for a migration example.