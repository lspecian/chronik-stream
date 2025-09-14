# Consumer Group Coordinator Implementation

## Overview

Consumer group coordination in Chronik Stream is now implemented using the WAL-based metadata system. This document describes the current architecture that replaced the previous controller-based approach.

## Current Architecture

### WAL-based Metadata Store

Consumer groups, offsets, and coordination state are now persisted through the Write-Ahead Log (WAL) system:

- **Group Metadata**: Consumer group information is stored as events in the metadata WAL
- **Offset Storage**: Consumer group offsets are persisted durably through WAL writes
- **Coordination**: Group coordination happens within the server process using the WAL for state persistence
- **Recovery**: On restart, group state is recovered by replaying WAL events

### Components

1. **Group Manager** (`crates/chronik-server/src/group_manager.rs`)
   - Manages consumer group lifecycle
   - Handles join/sync/heartbeat/leave operations
   - Coordinates rebalancing
   - Integrates with WAL-based metadata storage

2. **WAL Metadata Adapter** (`crates/chronik-storage/src/metadata_wal_adapter.rs`)
   - Provides metadata persistence through WAL
   - Implements event sourcing for group operations
   - Ensures durability of group state changes

3. **Chronik MetaLog** (`crates/chronik-common/src/metadata/wal_store.rs`)
   - Event-sourced metadata store
   - Handles topic, partition, and consumer group metadata
   - Provides ACID guarantees for metadata operations

## Key Features

- **Durability**: All group operations are persisted through WAL writes
- **Recovery**: Full state recovery on server restart
- **Performance**: Fast in-memory operations with durable persistence
- **Protocol Compliance**: Full Kafka protocol compatibility maintained
- **Simplicity**: Single-process architecture eliminates distributed consensus complexity

## Configuration

Enable WAL-based metadata storage:

```bash
chronik-server --wal-metadata --data-dir /path/to/wal/data
```

## Migration

The controller-based consumer group coordination system has been completely replaced by this WAL-based approach. No migration is needed for new deployments. The new system provides:

- Better performance (no network calls for metadata operations)
- Simplified deployment (single process instead of distributed controller)
- Stronger durability guarantees
- Easier debugging and monitoring

For detailed implementation information, see:
- [Chronik MetaLog Documentation](chronik-metalog.md)
- [WAL Performance Report](wal-performance-report.md)