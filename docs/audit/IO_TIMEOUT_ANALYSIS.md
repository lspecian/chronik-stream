# I/O Timeout Analysis - Phase 0 Task P0.1

**Date**: 2025-11-18
**Task**: Add Timeouts to All I/O Operations
**Estimated**: 8h
**Goal**: Prevent indefinite hangs in I/O operations

## Executive Summary

After analyzing the codebase, I've identified the following I/O operation categories and their current timeout status:

### ✅ Already Has Timeouts

**WAL Replication** ([wal_replication.rs](../../crates/chronik-server/src/wal_replication.rs)):
- ✅ TCP connection: 5s timeout (line 607, 1257)
- ✅ TCP read operations: 60s timeout (line 697, 1598)
- ✅ Frame completion: 30s timeout (line 747, 1647)
- ✅ Accept timeout: 1s (line 1508)

**Raft Operations** ([raft_cluster.rs](../../crates/chronik-server/src/raft_cluster.rs)):
- ✅ Proposal waiting: 5s timeout (line 702)
- ✅ Raft receiver: 10s timeout (line 1766)

**WAL Batch Operations** ([group_commit.rs](../../crates/chronik-wal/src/group_commit.rs)):
- ✅ Batch flush timeout: Configurable via `batch_timeout_ms`

### ❌ Missing Timeouts (CRITICAL)

**1. Disk I/O Operations**
- ❌ `file.write_all().await` - No timeout (can hang indefinitely on slow/failing disk)
- ❌ `file.sync_all().await` (fsync) - No timeout (can hang indefinitely)
- **Location**: [group_commit.rs](../../crates/chronik-wal/src/group_commit.rs):89, 103, 846, 852, 954, 1024
- **Risk**: HIGH - Disk failures or slow I/O can block the entire WAL subsystem
- **Impact**: Produce requests hang indefinitely, cluster becomes unresponsive

**2. Raft Lock Acquisitions**
- ❌ `raft_node.lock().await` - No timeout (can deadlock or block indefinitely)
- **Location**: [raft_cluster.rs](../../crates/chronik-server/src/raft_cluster.rs):594, 621, 750, 799, 848, 922, 1128, 1148, 1673, 1888, 2413
- **Risk**: MEDIUM-HIGH - Lock contention or deadlock can freeze Raft operations
- **Impact**: Metadata operations hang, cluster coordination fails

**3. Metadata Operations**
- ❌ Metadata WAL write - Inherits lack of timeout from disk I/O
- ❌ Raft proposal in metadata store - Some paths lack explicit timeout
- **Location**: Various metadata handlers
- **Risk**: MEDIUM - Can cause topic creation/deletion to hang
- **Impact**: Admin operations become unresponsive

**4. Client Connection Handling**
- ❌ Some client read operations may lack timeouts
- **Location**: Kafka protocol handlers
- **Risk**: LOW-MEDIUM - Slow clients can tie up server resources
- **Impact**: Resource exhaustion under load

## Implementation Plan

### Phase 1: Critical Disk I/O Timeouts (3h)

**Goal**: Prevent disk hang scenarios

**Approach**: Wrap disk I/O operations with `tokio::time::timeout`

```rust
use tokio::time::{timeout, Duration};

// Before:
file.write_all(data).await?;
file.sync_all().await?;

// After:
const DISK_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const DISK_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

timeout(DISK_WRITE_TIMEOUT, file.write_all(data))
    .await
    .context("Disk write timeout after 30s")?
    .context("Disk write I/O error")?;

timeout(DISK_SYNC_TIMEOUT, file.sync_all())
    .await
    .context("Disk fsync timeout after 60s")?
    .context("Disk fsync I/O error")?;
```

**Files to Modify**:
1. `crates/chronik-wal/src/group_commit.rs` - Add timeout wrappers to all disk I/O
2. `crates/chronik-wal/src/segment.rs` - Add timeout wrappers (if applicable)
3. `crates/chronik-server/src/metadata_wal.rs` - Ensure metadata WAL uses timeouts

**Configuration**:
```rust
pub struct DiskTimeoutConfig {
    /// Maximum time to wait for disk write (default: 30s)
    pub write_timeout_secs: u64,

    /// Maximum time to wait for fsync (default: 60s)
    pub sync_timeout_secs: u64,
}
```

### Phase 2: Raft Lock Timeouts (2h)

**Goal**: Prevent Raft deadlocks and long lock holds

**Approach**: Use `tokio::time::timeout` for lock acquisitions

```rust
// Before:
let raft = self.raft_node.lock().await;

// After:
const RAFT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

let raft = timeout(RAFT_LOCK_TIMEOUT, self.raft_node.lock())
    .await
    .context("Raft lock acquisition timeout after 5s")?;
```

**Files to Modify**:
1. `crates/chronik-server/src/raft_cluster.rs` - Add timeout to all lock acquisitions
2. `crates/chronik-server/src/raft_metadata_store.rs` - Add timeout (if file exists)

**Trade-offs**:
- Timeout too short: False positives under high load
- Timeout too long: Doesn't prevent hangs effectively
- **Recommendation**: 5s default, configurable via environment variable

### Phase 3: Client Connection Timeouts (1.5h)

**Goal**: Prevent slow clients from exhausting server resources

**Approach**: Add timeouts to client read/write operations

```rust
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

timeout(CLIENT_READ_TIMEOUT, client_stream.read_buf(&mut buffer))
    .await
    .context("Client read timeout")?
    .context("Client read error")?;
```

**Files to Check**:
1. `crates/chronik-server/src/kafka_handler.rs`
2. `crates/chronik-server/src/handler.rs`
3. `crates/chronik-protocol/src/frame.rs`

### Phase 4: Testing & Validation (1.5h)

**Goal**: Verify timeouts work correctly and don't cause false positives

**Test Scenarios**:
1. **Disk I/O timeout**:
   - Simulate slow disk (use `sleep` in test)
   - Verify timeout triggers and error propagates
   - Verify system remains responsive

2. **Raft lock timeout**:
   - Simulate lock contention (hold lock for >5s)
   - Verify timeout triggers
   - Verify no deadlocks

3. **Client timeout**:
   - Connect slow client
   - Verify timeout after configured period
   - Verify connection cleanup

**Integration Test**:
```rust
#[tokio::test]
async fn test_disk_write_timeout() {
    // TODO: Implement test that simulates slow disk
    // and verifies timeout behavior
}

#[tokio::test]
async fn test_raft_lock_timeout() {
    // TODO: Implement test that holds Raft lock
    // and verifies timeout on second acquisition
}
```

## Configuration Design

Add environment variables for timeout tuning:

```bash
# Disk I/O timeouts
CHRONIK_DISK_WRITE_TIMEOUT_SECS=30
CHRONIK_DISK_SYNC_TIMEOUT_SECS=60

# Raft operation timeouts
CHRONIK_RAFT_LOCK_TIMEOUT_SECS=5
CHRONIK_RAFT_PROPOSAL_TIMEOUT_SECS=10

# Client operation timeouts
CHRONIK_CLIENT_READ_TIMEOUT_SECS=30
CHRONIK_CLIENT_WRITE_TIMEOUT_SECS=30
```

## Risks and Mitigation

### Risk 1: Timeout Too Aggressive
**Symptom**: False positives under high load
**Mitigation**:
- Start with conservative defaults (30s write, 60s sync)
- Make configurable via environment variables
- Add metrics to track timeout frequency

### Risk 2: Timeout Hides Real Issues
**Symptom**: Disk failing but masked by timeout
**Mitigation**:
- Log all timeout events as WARN/ERROR
- Add metrics for timeout frequency
- Alert on high timeout rate

### Risk 3: Partial Writes on Timeout
**Symptom**: Data corruption if write times out mid-operation
**Mitigation**:
- WAL already has checksums to detect corruption
- Recovery mechanism can detect incomplete writes
- Timeout should fail-fast, not retry

## Metrics to Add

```rust
pub struct TimeoutMetrics {
    /// Number of disk write timeouts
    pub disk_write_timeouts: AtomicU64,

    /// Number of disk sync (fsync) timeouts
    pub disk_sync_timeouts: AtomicU64,

    /// Number of Raft lock acquisition timeouts
    pub raft_lock_timeouts: AtomicU64,

    /// Number of client read timeouts
    pub client_read_timeouts: AtomicU64,

    /// Number of client write timeouts
    pub client_write_timeouts: AtomicU64,
}
```

## Alternative Approaches Considered

### 1. OS-Level Timeouts
**Approach**: Use OS mechanisms (e.g., `SO_RCVTIMEO`, `SO_SNDTIMEO`)
**Pros**: Lower overhead, built-in
**Cons**: Less flexible, harder to configure
**Decision**: Rejected - Tokio timeout is more Rust-idiomatic

### 2. Deadline-Based API
**Approach**: Pass deadline to all I/O operations
**Pros**: More precise control
**Cons**: API complexity, requires significant refactoring
**Decision**: Rejected - Too invasive for Phase 0

### 3. No Timeouts (Status Quo)
**Approach**: Rely on external monitoring to detect hangs
**Pros**: Simpler code
**Cons**: Doesn't prevent issues, only detects them
**Decision**: Rejected - Phase 0 goal is stability improvement

## Success Criteria

- ✅ All disk I/O operations have configurable timeouts
- ✅ All Raft lock acquisitions have timeouts
- ✅ All client I/O operations have timeouts
- ✅ Tests verify timeout behavior under failure scenarios
- ✅ Metrics track timeout frequency
- ✅ Documentation updated with timeout configuration
- ✅ No false positives in normal operation
- ✅ System remains responsive under disk/network failures

## Implementation Priority

1. **Disk I/O timeouts** (CRITICAL) - Can hang entire system
2. **Raft lock timeouts** (HIGH) - Can cause metadata operations to hang
3. **Client connection timeouts** (MEDIUM) - Can exhaust resources
4. **Testing & validation** (HIGH) - Ensure no regressions

## Next Steps

1. Implement Phase 1 (Disk I/O timeouts) in `group_commit.rs`
2. Test with integration tests
3. Implement Phase 2 (Raft lock timeouts) in `raft_cluster.rs`
4. Implement Phase 3 (Client timeouts) if time permits
5. Update IMPLEMENTATION_TRACKER.md with completion

## References

- **P0.5 Investigation**: Discovered Raft batching bottleneck (fixed in v2.2.9)
- **P0.4 Fix**: Cluster startup deadlock (already fixed)
- **P0.3**: Verify v2.2.7 deadlock fix (separate task)
