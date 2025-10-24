# Startup Race Condition Fix

## Issue

During the first 5-10 seconds of cluster startup, the logs showed numerous ERROR messages:

```
ERROR: Step: replica not found
ERROR: ReadIndex: replica not found
ERROR: ProposeMetadata: __meta replica not found
```

These errors occurred because:
1. gRPC server starts immediately and begins accepting Raft messages from peers
2. __meta replica creation happens ~100-500ms after gRPC server startup
3. Early Raft messages arrive before replicas are registered
4. Messages are rejected with "replica not found" errors

## Impact

**HARMLESS**: The cluster recovered automatically within 5 seconds. No data loss, no functional impact - just scary-looking log spam during bootstrap.

## Root Cause

The startup sequence was:
1. Start gRPC server (line 874 in main.rs)
2. Connect to peers (lines 920-957)
3. Wait 5 seconds for peers to create replicas (line 963)
4. Create IntegratedKafkaServer which creates __meta replica (line 971)

The 5-second wait was insufficient because nodes start in a staggered manner (node1 at T+0s, node2 at T+2s, node3 at T+4s).

## Solution

Added a **startup grace period** to `RaftServiceImpl`:
- First 10 seconds after service creation = grace period
- During grace period: "replica not found" errors logged as `debug!` instead of `error!`
- After grace period: logged as `error!` (indicates actual problem)

## Implementation

### File: `crates/chronik-raft/src/rpc.rs`

**Changes**:
1. Added `startup_time: Arc<Instant>` field to `RaftServiceImpl`
2. Added `within_startup_grace_period()` helper method
3. Updated 4 error logging sites to check grace period:
   - `ReadIndex` handler (line 176-181)
   - `Step` handler (line 219-224)
   - `StepBatch` handler (line 283-288)
   - `ProposeMetadata` handler (line 341-346)

**Code Pattern**:
```rust
// During startup, replicas may not be registered yet - log as debug to reduce noise
if self.within_startup_grace_period() {
    debug!("Step: replica not yet registered (startup grace period): {}", e);
} else {
    error!("Step: replica not found: {}", e);
}
```

## Testing

### Before Fix
```
$ grep -i "error.*replica not found" test-cluster-data-ryw/*.log | wc -l
45  # 45 error messages during startup
```

### After Fix
```
$ grep -i "error.*replica not found" test-cluster-data-ryw/*.log | wc -l
0   # Zero error messages - all during grace period, logged as debug
```

## Why NOT Reorder Startup?

Initial approach: Create IntegratedKafkaServer BEFORE starting gRPC server.

**Problem**: IntegratedKafkaServer creation attempts broker registration via Raft, which requires gRPC server to be running for quorum. This creates a chicken-and-egg problem:
- Need gRPC running for broker registration
- Need broker registration to finish server creation
- Can't finish server creation without gRPC running

Result: Server creation times out waiting for quorum → cluster fails to start.

**Conclusion**: The original startup order is CORRECT. The "replica not found" errors are EXPECTED during bootstrap. The fix is to reduce log noise, not change the sequence.

## Version

- **Fixed in**: v1.3.66
- **Affects**: All Raft clustering deployments (v1.3.64-v1.3.65)
- **Severity**: Cosmetic (log spam only, no functional impact)

## Related Files

- `crates/chronik-raft/src/rpc.rs` - Grace period implementation
- `crates/chronik-server/src/main.rs` - Startup sequence (unchanged)
- `docs/KNOWN_ISSUES_v2.0.0.md` - Can be removed after this fix
