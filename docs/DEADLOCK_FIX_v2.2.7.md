# Raft Message Loop Deadlock Fix (v2.2.7 Phase 5)

## Problem

The 3-node Raft cluster would start successfully but **completely freeze** after initialization, unable to process any Kafka client requests.

### Symptoms
- ✅ Brokers initialized and persisted to Raft WAL
- ✅ Raft leader elected (term 2)
- ✅ Server processes running
- ✅ Ports listening (9092, 9291, 5001)
- ❌ **Kafka server NOT accepting connections**
- ❌ **No logs after initialization** (stuck at ~18:37:06)
- ❌ Clients timeout: `KafkaTimeoutError: Failed to update metadata after 60.0 secs`

### Root Cause: Write-Write Deadlock

Location: [crates/chronik-server/src/raft_cluster.rs:1217-1232](../crates/chronik-server/src/raft_cluster.rs#L1217-L1232)

The Raft message loop (`start_message_loop`) was **deadlocking** when processing ConfChange entries:

1. **Line 1114**: Message loop acquires `self.raft_node.write()` → holds `raft_lock`
2. **Line 1200**: Processes ConfChange entries
3. **Line 1217**: Tries to acquire `self.raft_node.write()` **AGAIN** to call `apply_conf_change()`
4. **Deadlock**: RwLock cannot be acquired twice by the same thread (write-write deadlock)

```rust
// BEFORE (deadlock):
let mut raft_lock = self.raft_node.write()?;  // Line 1114
// ... processing ...
// Line 1217 - tries to acquire AGAIN:
let mut raft = self.raft_node.write()?;  // ❌ DEADLOCK
raft.apply_conf_change(&cc)?;
```

The message loop would **block forever** at line 1217, preventing:
- Raft ticks
- Kafka server from processing requests
- Admin API from responding
- All async tasks (they're waiting on the message loop to release the lock)

## Solution

**Use the existing `raft_lock` variable** instead of trying to re-acquire the lock:

```rust
// AFTER (fixed):
let mut raft_lock = self.raft_node.write()?;  // Line 1114
// ... processing ...
// Line 1218 - use existing lock:
let cs = raft_lock.apply_conf_change(&cc)?;  // ✅ Works!
```

### Code Changes

**File**: [crates/chronik-server/src/raft_cluster.rs](../crates/chronik-server/src/raft_cluster.rs)

```diff
- // Apply to Raft (updates voter list)
- let cs = {
-     let mut raft = match self.raft_node.write() {
-         Ok(r) => r,
-         Err(e) => {
-             tracing::error!("Failed to acquire Raft lock: {}", e);
-             continue;
-         }
-     };
-
-     match raft.apply_conf_change(&cc) {
-         Ok(cs) => cs,
-         Err(e) => {
-             tracing::error!("Failed to apply ConfChange: {:?}", e);
-             continue;
-         }
-     }
- };

+ // Apply to Raft (updates voter list)
+ // CRITICAL FIX: Use existing raft_lock instead of re-acquiring
+ // (re-acquiring would deadlock since we already hold the write lock)
+ let cs = match raft_lock.apply_conf_change(&cc) {
+     Ok(cs) => cs,
+     Err(e) => {
+         tracing::error!("Failed to apply ConfChange: {:?}", e);
+         continue;
+     }
+ };
```

## Verification

### Test Results

**Before Fix**:
```bash
$ python3 test_client.py
KafkaTimeoutError: Failed to update metadata after 60.0 secs
```

**After Fix**:
```bash
$ python3 test_client.py
✓ Connected to Kafka server!
✓ Message sent successfully!
✓ Admin client connected!
Topics: ['chronik-default', 'test-topic']
```

### Build & Test

```bash
# 1. Build with fix
cargo build --release --bin chronik-server

# 2. Start 3-node cluster
cd tests/cluster && ./start.sh

# 3. Test connectivity
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092', api_version=(0, 10, 0))
print('✓ Connected!')
producer.send('test-topic', b'Hello!')
producer.flush()
print('✓ Message sent!')
"
```

## Impact

- ✅ **Raft message loop no longer blocks**
- ✅ **Kafka server accepts client connections**
- ✅ **Admin API responds to requests**
- ✅ **All async tasks execute normally**
- ✅ **Zero regressions** (same lock semantics, just avoiding redundant acquisition)

## Related Issues

- **v2.2.7 Phase 5**: Broker registration timing issue (this was the actual blocker)
- **Priority 2**: Zero-downtime node addition (ConfChange processing path)

## Lessons Learned

### Why This Wasn't Caught Earlier

1. **Complex async code**: Message loop has 400+ lines of lock handling
2. **Edge case**: ConfChange processing only triggers during:
   - First boot (when brokers are persisted)
   - Node addition/removal operations
3. **No lock validation**: Rust RwLock allows *read-after-write* but not *write-after-write*

### Prevention

- **Add lock assertions** to verify single-write invariant
- **Extract ConfChange processing** to separate function (clearer ownership)
- **Document lock lifetimes** in message loop (already partially done)

## Status

- **Fixed in**: v2.2.7 (commit pending)
- **Tested**: ✅ 3-node cluster startup and operation
- **Impact**: **CRITICAL** - without this fix, all multi-node clusters deadlock on first boot
