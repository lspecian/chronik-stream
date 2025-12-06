# Known Issues and Code TODOs

This document tracks known bugs, incomplete implementations, and code TODOs in the Chronik Stream codebase.

**Last Updated**: 2025-12-02
**Version**: v2.2.19

---

## Fixed Issues

### Bug #4: ListOffsets Timeout on Followers - FIXED (v2.2.9)
- **Location**: `record_processor.rs:162`, `wal_replication.rs:2041`
- **Problem**: Followers had correct ProduceHandler watermarks but metadata_store offsets were 0
- **Symptom**: ListOffsets API would timeout on follower nodes
- **Fix**: During WAL replication, also update MetadataStore partition offsets

### Bug #5: Metadata WAL Cold-Start Recovery - FIXED (v2.2.19)
- **Location**: `metadata_wal.rs:277-460`
- **Problem**: `read_all_wal_records()` was disabled, returning empty vector
- **Symptom**: After full cluster cold-start, all metadata (topics, partitions, offsets) was lost
- **Fix**: Implemented proper WAL record parsing using same format as `WalManager.read_from()`

### Checkpoint CRC Calculation - FIXED (v2.2.20)
- **Location**: `crates/chronik-wal/src/checkpoint.rs:112-126`
- **Problem**: CRC field was hardcoded to 0
- **Fix**: Now calculates CRC32 over offset + timestamp bytes

### Benchmark Round-Trip and Metadata Modes - IMPLEMENTED (v2.2.20)
- **Location**: `crates/chronik-bench/src/benchmark.rs`
- **Problem**: Round-trip and metadata benchmark modes were stubs
- **Fix**: Implemented both modes:
  - Round-trip: Embeds timestamps in payloads, measures end-to-end latency
  - Metadata: Creates/deletes topics, measures metadata operation latency

### Rebalance CLI Command - IMPLEMENTED (v2.2.20)
- **Location**: `crates/chronik-server/src/main.rs`, `crates/chronik-server/src/admin_api.rs`
- **Problem**: CLI `cluster rebalance` command was a stub
- **Fix**: Implemented full rebalance via admin API:
  - Added `/admin/rebalance` POST endpoint
  - CLI finds cluster leader and sends rebalance request
  - Uses round-robin algorithm across all nodes

---

## Open Issues

### P1: Critical

*None currently*

### P2: Important

#### Checkpoint segment_id/position Not Populated
- **Location**: `crates/chronik-wal/src/checkpoint.rs:122-123`
- **Problem**: Checkpoint struct uses placeholder values for segment tracking:
  ```rust
  segment_id: 0,  // Placeholder: will be set when checkpoint-based seek is implemented
  position: 0,    // Placeholder: will be set when checkpoint-based seek is implemented
  ```
- **Impact**: Checkpoint-based segment seek optimization not available
- **Fix**: Wire up actual segment state when implementing checkpoint-based recovery

### P3: Minor

#### Admin API ISR Query Uses All Replicas
- **Location**: `crates/chronik-server/src/admin_api.rs:349`
- **Problem**: Returns `isr: replicas.clone()` - assumes all replicas are in-sync
- **Impact**: Cluster status shows ISR = replicas even when some replicas may be lagging
- **Root Cause**: The `IsrTracker` module exists but isn't wired up because proper ISR
  tracking requires a heartbeat protocol where followers report their log end offsets
  back to the leader. Currently, the WAL replication is one-way (leader → followers).
- **Fix Required**:
  1. Implement follower → leader offset reporting (e.g., via periodic RPCs)
  2. Create IsrTracker in builder and pass to WalReplicationManager
  3. Have WalReceiver update IsrTracker when records are received
  4. Add IsrTracker to AdminApiState and query in collect_partition_info_from_metadata()
- **Workaround**: For most production use cases, ISR = replicas is acceptable when the
  cluster is healthy. Monitor WAL replication lag separately if needed.

#### Streaming WAL Read Not Implemented
- **Location**: `crates/chronik-wal/src/manager.rs:529`
- **Problem**: `TODO: Implement streaming read` - currently reads entire WAL into memory
- **Impact**: High memory usage for large WAL files
- **Fix**: Implement iterator-based streaming read

### P4: Low Priority

#### Consumer Group Client ID Hardcoded
- **Location**: `crates/chronik-server/src/consumer_group.rs:1678`
- **Problem**: Uses hardcoded `/127.0.0.1` instead of parsing from connection
- **Impact**: Consumer group metadata shows incorrect client info
- **Fix**: Parse actual client address from connection info

#### Protocol Handler Host/Port Hardcoded
- **Location**: `crates/chronik-protocol/src/handler.rs:5491-5492`
- **Problem**: Returns hardcoded `localhost:9092` instead of actual config
- **Impact**: Metadata responses may have incorrect broker info
- **Fix**: Get host/port from server configuration

---

## Code Quality Issues

### DEBUG Logs in Production Code
Several files contain `DEBUG:` prefixed log messages that should be cleaned up:
- `consumer_group.rs:1764, 1843, 1918`
- `record_processor.rs:152, 156, 316, 344`
- `consumer_group/assignment.rs:44`

These are harmless but add noise to logs. Consider removing or converting to TRACE level.

---

## How to Contribute

1. Pick an issue from the list above
2. Create a branch: `git checkout -b fix/issue-name`
3. Implement the fix with tests
4. Update this document to move the issue to "Fixed Issues"
5. Submit PR with reference to this document

For questions, see [CLAUDE.md](../CLAUDE.md) for project conventions.
