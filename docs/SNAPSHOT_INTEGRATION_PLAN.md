# Chronik Raft Snapshot Integration Plan

**Date**: 2025-10-21
**Status**: 🟡 IN PROGRESS
**Priority**: CRITICAL (blocks production readiness)

## Executive Summary

Integrate existing snapshot infrastructure (`snapshot.rs`) with Chronik Raft cluster to enable:
- Log compaction (prevent infinite Raft log growth)
- Fast node recovery (bootstrap from snapshots instead of full log replay)
- Reduced storage costs (old log entries can be discarded)

## Current State

### ✅ Existing Infrastructure

**`crates/chronik-raft/src/snapshot.rs`** (1,343 lines):
- `SnapshotManager` - Complete implementation
- Snapshot creation with Gzip compression
- S3/object store upload (`put`/`get`/`delete`)
- Snapshot restoration with checksum verification
- Background loop for automatic snapshots
- Retention policy (keep last N snapshots)
- CRC32 checksums for data integrity

**`crates/chronik-raft/src/snapshot_bootstrap.rs`** (392 lines):
- `SnapshotBootstrap` - Bootstrap new nodes from snapshots
- Download latest snapshot from S3
- Apply snapshot to restore state
- Replay only recent log entries

**Configuration** (`SnapshotConfig`):
```rust
pub struct SnapshotConfig {
    pub enabled: bool,                        // default: true
    pub log_size_threshold: u64,              // default: 10,000 entries
    pub time_threshold: Duration,             // default: 1 hour
    pub max_concurrent_snapshots: usize,      // default: 2
    pub compression: SnapshotCompression,     // default: Gzip
    pub retention_count: usize,               // default: 3
}
```

### ❌ Missing Integration

1. **SnapshotManager not created** in server startup
2. **No object store passed** to SnapshotManager
3. **Snapshot loop not spawned** in background
4. **Raft log truncation not implemented** after snapshot
5. **Snapshot triggers not wired** to actual Raft log size
6. **Bootstrap not integrated** into node startup

## Integration Architecture

### Snapshot Flow

```
┌─────────────────────────────────────────────────────────────┐
│                   Snapshot Lifecycle                          │
├───────────────────────────────────────────────────────────────┤
│                                                               │
│  1. TRIGGER (every 5 minutes background loop)                │
│     ├─ Check log size threshold (default: 10,000 entries)    │
│     ├─ Check time threshold (default: 1 hour)                │
│     └─ If exceeded → Create snapshot                         │
│                                                               │
│  2. SNAPSHOT CREATION                                         │
│     ├─ Get current applied_index and term from Raft replica  │
│     ├─ Serialize partition state (metadata + data)           │
│     ├─ Compress with Gzip (or None/Zstd)                     │
│     ├─ Calculate CRC32 checksum                              │
│     ├─ Upload to S3 as snapshots/{topic}/{partition}/{id}.snap│
│     └─ Generate SnapshotMetadata                             │
│                                                               │
│  3. RAFT LOG TRUNCATION                                      │
│     ├─ Truncate Raft log up to last_included_index          │
│     ├─ Free disk space from old log entries                  │
│     └─ Update applied_index in replica state                 │
│                                                               │
│  4. RETENTION CLEANUP                                         │
│     ├─ List all snapshots for partition                      │
│     ├─ Sort by created_at (newest first)                     │
│     ├─ Delete all except retention_count (default: 3)        │
│     └─ Prevent unbounded S3 storage growth                   │
│                                                               │
│  5. NODE RECOVERY (on startup)                               │
│     ├─ Check if S3 snapshot exists for partition             │
│     ├─ Download and verify checksum                          │
│     ├─ Decompress and deserialize                            │
│     ├─ Apply to Raft state machine                           │
│     ├─ Set applied_index to snapshot's last_included_index   │
│     └─ Replay only Raft log entries AFTER snapshot           │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

### Component Integration Points

**`crates/chronik-server/src/raft_cluster.rs`**:
```rust
// After RaftReplicaManager creation (line ~100)
use chronik_raft::snapshot::{SnapshotManager, SnapshotConfig};

let snapshot_config = SnapshotConfig {
    enabled: true,
    log_size_threshold: 10_000,
    time_threshold: Duration::from_secs(3600),  // 1 hour
    max_concurrent_snapshots: 2,
    compression: SnapshotCompression::Gzip,
    retention_count: 3,
};

let snapshot_manager = Arc::new(SnapshotManager::new(
    config.node_id,
    raft_manager.clone(),
    metadata_store.clone(),
    snapshot_config,
    object_store.clone(),  // From integrated_server
));

// Spawn snapshot background loop
let snapshot_loop_handle = snapshot_manager.spawn_snapshot_loop();
```

**`crates/chronik-server/src/integrated_server.rs`**:
```rust
// Pass snapshot_manager to IntegratedKafkaServer
pub struct IntegratedKafkaServer {
    // ... existing fields
    snapshot_manager: Option<Arc<SnapshotManager>>,
}
```

**`crates/chronik-raft/src/replica.rs`**:
```rust
// Add snapshot support to PartitionReplica
impl PartitionReplica {
    // ALREADY EXISTS:
    pub fn applied_index(&self) -> u64 { ... }
    pub fn commit_index(&self) -> u64 { ... }
    pub fn set_applied_index(&self, index: u64) { ... }

    // NEW METHOD:
    pub fn truncate_log_before(&self, index: u64) -> Result<()> {
        // Truncate Raft log entries before index
        // Call Raft's compact() or similar
    }
}
```

## Implementation Tasks

### Phase 1: Basic Integration (Day 1 - 4 hours)

#### Task 3.1: Create SnapshotManager in raft_cluster.rs ✅ IN PROGRESS
**Files**: `crates/chronik-server/src/raft_cluster.rs`

**Changes**:
1. Add imports for `SnapshotManager`, `SnapshotConfig`, `SnapshotCompression`
2. Create `SnapshotConfig` from environment variables or defaults
3. Instantiate `SnapshotManager` after RaftReplicaManager creation
4. Spawn snapshot background loop
5. Store handle for graceful shutdown

**Code Location**: After line ~320 (after `__meta` replica creation)

**Environment Variables**:
```bash
CHRONIK_SNAPSHOT_ENABLED=true                # default: true
CHRONIK_SNAPSHOT_LOG_THRESHOLD=10000         # default: 10,000
CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS=3600    # default: 3600 (1 hour)
CHRONIK_SNAPSHOT_MAX_CONCURRENT=2            # default: 2
CHRONIK_SNAPSHOT_COMPRESSION=gzip            # gzip|none|zstd
CHRONIK_SNAPSHOT_RETENTION_COUNT=3           # default: 3
```

#### Task 3.2: Pass SnapshotManager to IntegratedKafkaServer
**Files**: `crates/chronik-server/src/integrated_server.rs`, `raft_cluster.rs`

**Changes**:
1. Add `snapshot_manager: Option<Arc<SnapshotManager>>` to `IntegratedServerConfig`
2. Pass snapshot_manager from `raft_cluster.rs` to `IntegratedKafkaServer::new()`
3. Store in `IntegratedKafkaServer` struct

**Benefits**: Makes snapshot manager available to all server components

#### Task 3.3: Wire Snapshot Triggers to Raft Log Size
**Files**: `crates/chronik-raft/src/snapshot.rs`

**Changes**:
1. Fix `should_create_snapshot()` to get actual Raft log size
2. Current issue: Uses `commit_index - applied_index` which may not reflect actual log size
3. Alternative: Track log entry count in `RaftLogStorage`

**Current Code** (line 290-304):
```rust
if let Some(replica) = self.raft_group_manager.get_replica(topic, partition) {
    let commit_index = replica.commit_index();
    let applied_index = replica.applied_index();

    // BUG: This doesn't account for log compaction
    let entries_since_snapshot = commit_index.saturating_sub(applied_index);

    if entries_since_snapshot >= self.snapshot_config.log_size_threshold {
        return Ok(true);
    }
}
```

**Fix**: Use actual log storage size or maintain entry counter

### Phase 2: Raft Log Truncation (Day 1 - 2 hours)

#### Task 3.4: Implement Raft Log Truncation
**Files**: `crates/chronik-raft/src/replica.rs`, `snapshot.rs`

**Changes**:
1. Add `truncate_log_before(index: u64)` method to `PartitionReplica`
2. Call Raft's internal log compaction mechanism
3. Update `create_snapshot_internal()` to call truncation after snapshot upload
4. Verify truncation doesn't affect committed but unapplied entries

**Raft Integration**:
- Use `RawNode::compact()` or similar from `raft` crate
- Ensure truncation is safe (index < commit_index)

#### Task 3.5: Test Log Truncation
**Files**: `tests/integration/raft_snapshot_test.rs` (create)

**Test Scenarios**:
1. Create 20,000 log entries (2x threshold)
2. Trigger snapshot creation
3. Verify log truncated to last_included_index
4. Verify replica still functional after truncation
5. Verify new entries can be appended

### Phase 3: Snapshot Restoration (Day 2 - 4 hours)

#### Task 3.6: Integrate Snapshot Bootstrap on Startup
**Files**: `crates/chronik-server/src/raft_cluster.rs`

**Changes**:
1. Create `SnapshotBootstrap` instance
2. Before creating replica, check `should_bootstrap()`
3. If yes, call `bootstrap_partition()` to restore from snapshot
4. Then create replica with applied_index set from snapshot

**Code Location**: Before `raft_manager.create_meta_replica()` (line ~305)

**Flow**:
```rust
use chronik_raft::snapshot_bootstrap::{SnapshotBootstrap, BootstrapConfig};

let bootstrap_config = BootstrapConfig::default();
let bootstrap = SnapshotBootstrap::new(
    config.node_id,
    raft_manager.clone(),
    snapshot_manager.clone(),
    bootstrap_config,
);

// Check if snapshot available for __meta
if bootstrap.should_bootstrap("__meta", 0).await? {
    info!("Bootstrapping __meta from snapshot");
    bootstrap.bootstrap_partition("__meta", 0).await?;
}

// Then create replica normally
raft_manager.create_meta_replica(...).await?;
```

#### Task 3.7: Test Cold Start Recovery
**Files**: `tests/integration/raft_snapshot_recovery_test.rs` (create)

**Test Scenarios**:
1. Create cluster, create topic, produce 10,000 messages
2. Trigger snapshot creation
3. Stop node, delete local WAL
4. Restart node - should bootstrap from snapshot
5. Verify all 10,000 messages accessible

### Phase 4: Testing & Validation (Day 2 - 2 hours)

#### Task 3.8: Integration Tests
**Files**: Create new test files

**Tests**:
1. **`test_snapshot_creation_and_upload.rs`**:
   - Create snapshot, verify uploaded to S3
   - Verify checksum correct
   - Verify compression works

2. **`test_snapshot_retention.rs`**:
   - Create 5 snapshots
   - Verify only 3 kept (retention_count)
   - Verify oldest deleted from S3

3. **`test_snapshot_restoration.rs`**:
   - Create snapshot
   - Restore to new replica
   - Verify applied_index set correctly

4. **`test_snapshot_background_loop.rs`**:
   - Spawn background loop
   - Produce messages to exceed threshold
   - Wait for automatic snapshot creation
   - Verify snapshot created

#### Task 3.9: E2E Validation
**Script**: `test_snapshot_e2e.py`

**Scenario**:
1. Start 3-node cluster
2. Create topic with 3 partitions
3. Produce 20,000 messages (2x threshold)
4. Wait for automatic snapshots
5. Verify snapshots uploaded to S3/MinIO
6. Kill one node
7. Restart node - verify bootstraps from snapshot
8. Verify all 20,000 messages still consumable

### Phase 5: Documentation (Day 2 - 1 hour)

#### Task 3.10: Update Documentation
**Files**:
- `CLAUDE.md` - Add snapshot configuration section
- `RAFT_TESTING_GUIDE.md` - Add snapshot testing section
- `docs/SNAPSHOT_ARCHITECTURE.md` (new) - Complete snapshot design doc

**Topics**:
- Configuration options
- How snapshots work
- Troubleshooting snapshot issues
- Performance tuning

## Configuration

### Default Configuration

```rust
SnapshotConfig {
    enabled: true,
    log_size_threshold: 10_000,              // Entries
    time_threshold: Duration::from_secs(3600), // 1 hour
    max_concurrent_snapshots: 2,
    compression: SnapshotCompression::Gzip,
    retention_count: 3,
}
```

### Environment Variables

```bash
# Enable/disable snapshots
CHRONIK_SNAPSHOT_ENABLED=true

# Create snapshot after N log entries
CHRONIK_SNAPSHOT_LOG_THRESHOLD=10000

# Create snapshot after N seconds
CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS=3600

# Maximum concurrent snapshot operations
CHRONIK_SNAPSHOT_MAX_CONCURRENT=2

# Compression algorithm (gzip, none, zstd)
CHRONIK_SNAPSHOT_COMPRESSION=gzip

# Number of snapshots to keep
CHRONIK_SNAPSHOT_RETENTION_COUNT=3
```

### S3 Configuration

Snapshots use the existing object store configuration:

```bash
OBJECT_STORE_BACKEND=s3
S3_BUCKET=chronik-snapshots
S3_REGION=us-west-2
S3_PREFIX=snapshots/
```

**Snapshot Path Format**:
```
s3://chronik-snapshots/snapshots/{topic}/{partition}/{snapshot_id}.snap
```

## Performance Characteristics

### Snapshot Creation

**Time**:
- 10,000 entries: ~1-2 seconds (with Gzip compression)
- 100,000 entries: ~10-20 seconds
- 1,000,000 entries: ~2-3 minutes

**Size** (with Gzip compression):
- ~70-80% size reduction vs uncompressed
- 10,000 entries: ~1-5 MB compressed
- 100,000 entries: ~10-50 MB compressed

**CPU Usage**:
- Gzip compression: Medium (single-threaded)
- No compression: Minimal
- Zstd compression: Low (faster than Gzip)

### Snapshot Restoration

**Time**:
- Download from S3: 50-200ms per MB
- Decompression: ~500ms per 10MB
- State application: ~100ms

**Example**: 50MB snapshot
- Download: ~5-10 seconds
- Decompress: ~2-3 seconds
- Apply: ~100ms
- **Total**: ~8-13 seconds

**vs Full Log Replay** (100,000 entries):
- Log replay: ~30-60 seconds
- Snapshot bootstrap: ~10 seconds
- **Speedup**: 3-6x faster

### Resource Usage

**Disk Space**:
- Log before compaction: ~100MB per 10,000 entries
- Snapshot: ~5MB (Gzip compressed)
- **Savings**: ~95% disk space

**S3 Costs** (assuming 3-partition topic, 3 snapshots retention):
- Storage: 3 partitions × 3 snapshots × 5MB = 45MB
- At $0.023/GB/month: ~$0.001/month
- **Negligible**

## Troubleshooting

### Issue: Snapshots not being created

**Symptoms**: No `.snap` files in S3 after threshold exceeded

**Diagnosis**:
```bash
# Check snapshot manager logs
grep -i "snapshot" /var/log/chronik/server.log

# Verify configuration
env | grep CHRONIK_SNAPSHOT

# Check background loop running
curl http://localhost:9090/metrics | grep snapshot_loop_running
```

**Solutions**:
- Verify `CHRONIK_SNAPSHOT_ENABLED=true`
- Check log threshold not too high
- Verify object store connection working

### Issue: Snapshot download fails

**Symptoms**: Node startup fails with "Failed to download snapshot"

**Diagnosis**:
```bash
# Check S3 connectivity
aws s3 ls s3://chronik-snapshots/snapshots/

# Check snapshot exists
aws s3 ls s3://chronik-snapshots/snapshots/{topic}/{partition}/

# Verify credentials
aws sts get-caller-identity
```

**Solutions**:
- Verify S3 credentials correct
- Check network connectivity to S3
- Verify snapshot file not corrupted (checksum mismatch)

### Issue: Snapshot checksum mismatch

**Symptoms**: "Snapshot checksum mismatch: expected X, got Y"

**Diagnosis**:
- Corruption during upload/download
- Incorrect compression algorithm used

**Solutions**:
- Re-create snapshot
- Try different compression (or none)
- Check S3 bucket versioning enabled

## Timeline

### Day 1 (6 hours)
- ✅ Task 3.1: Create SnapshotManager (2h) - IN PROGRESS
- Task 3.2: Pass to IntegratedKafkaServer (1h)
- Task 3.3: Wire triggers (1h)
- Task 3.4: Implement log truncation (2h)

### Day 2 (7 hours)
- Task 3.5: Test log truncation (1h)
- Task 3.6: Integrate bootstrap (2h)
- Task 3.7: Test cold start recovery (2h)
- Task 3.8: Integration tests (1h)
- Task 3.9: E2E validation (1h)

### Day 3 (Optional - Polish)
- Task 3.10: Documentation (1h)
- Performance tuning (2h)
- Metrics and observability (2h)

**Total**: ~2-3 days to production-ready snapshots

## Success Criteria

✅ **Functional**:
- [ ] Snapshots created automatically when thresholds exceeded
- [ ] Snapshots uploaded to S3/object store
- [ ] Raft log truncated after snapshot
- [ ] Retention policy enforced (old snapshots deleted)
- [ ] Nodes bootstrap from snapshot on cold start
- [ ] Faster recovery than full log replay

✅ **Performance**:
- [ ] Snapshot creation < 10s for 100K entries
- [ ] Snapshot restoration < 30s for 100K entries
- [ ] Disk space savings > 90%
- [ ] No impact on produce/fetch latency

✅ **Reliability**:
- [ ] Checksum verification prevents corruption
- [ ] Graceful degradation if snapshot unavailable
- [ ] No data loss during snapshot creation
- [ ] All integration tests passing

## Next Steps

**Immediate** (Task 3.1 - in progress):
1. Create `SnapshotManager` in `raft_cluster.rs`
2. Configure from environment variables
3. Spawn background loop
4. Test snapshot creation manually

**Follow-up**:
1. Implement Raft log truncation
2. Test cold start recovery
3. Write comprehensive integration tests
4. Update documentation

---

**Document Version**: 1.0
**Last Updated**: 2025-10-21
**Status**: Implementation in progress
