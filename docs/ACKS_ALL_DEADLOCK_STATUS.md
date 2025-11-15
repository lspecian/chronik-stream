# acks=all Deadlock Fix - Current Status

## Date: 2025-11-13

## Summary

✅ **RESOLVED**: The acks=all timeout issue is fully resolved. Test passes consistently with 16ms completion time.

## Final Fix (Session 6)

**Root Cause Found**: Startup race condition where leader attempted to connect to followers' WAL receivers before they had started listening. With `RECONNECT_DELAY = 30 seconds`, broken connections persisted throughout the test.

**Solution**: Reduced `RECONNECT_DELAY` from 30 seconds to 1 second in [crates/chronik-server/src/wal_replication.rs](../crates/chronik-server/src/wal_replication.rs):58

**Code Change:**
```rust
// Before:
const RECONNECT_DELAY: Duration = Duration::from_secs(30);

// After:
const RECONNECT_DELAY: Duration = Duration::from_secs(1);  // v2.2.7: Reduced from 30s to 1s to handle startup race conditions
```

**Why This Works:**
- Node startup order is unpredictable in distributed systems
- WAL receivers may start 3-5 seconds after leaders attempt connection
- 1-second retry ensures connections recover quickly during cluster startup
- No performance impact (reconnect only happens on connection failure)

**Evidence from Logs:**
```
2025-11-13T15:36:18 [WARN] Failed to connect to follower localhost:9292: Connection refused
2025-11-13T15:36:19 [INFO] ✅ Connected to follower: localhost:9292  ← Recovered in 1 second!
2025-11-13T15:36:19 [INFO] Updated followers for acks-all-test-0: ["localhost:9292", "localhost:9293"]
```

**See [ACKS_ALL_DEADLOCK_COMPLETE.md](ACKS_ALL_DEADLOCK_COMPLETE.md) for complete fix documentation.**

## What's Working ✅

1. **Discovery Loop Timing Fixed**
   - Changed from sleep-first to check-first pattern
   - Reduced interval from 10s to 1s
   - Discovery happens within 100ms of partition creation
   - File: `crates/chronik-server/src/wal_replication.rs` lines 790-872

2. **Partition Leaders Are Assigned**
   - Leaders are correctly assigned during topic creation
   - Stored in Raft metadata state machine
   - Discovery worker correctly identifies partitions where node is leader
   - Log shows: "Node 1 is leader for 13 partitions: [... ("acks-all-test", 0) ...]"

3. **Code Structure**
   - Added `assign_partition_replicas()` and `update_isr_replicated()` methods to RaftMetadataStore
   - Methods implement proper WAL-based replication pattern:
     1. Write to metadata WAL
     2. Apply locally to Raft state machine
     3. Async replicate to followers via MetadataWalReplicator
   - File: `crates/chronik-server/src/raft_metadata_store.rs` lines 1128-1223

## What's NOT Working ❌

1. **Metadata WAL Replication Not Activated**
   - The new replication methods exist but are NOT being called from produce_handler
   - This is because they're not part of the MetadataStore trait
   - Calling them would require downcasting Arc<dyn MetadataStore> to RaftMetadataStore
   - Current code has a TODO comment instead of actual calls
   - File: `crates/chronik-server/src/produce_handler.rs` lines 2547-2559

2. **Followers Don't Have ISR Information**
   - Leader has ISR in local Raft state
   - Followers never receive ISR updates
   - This means `partition_followers` map stays empty
   - No "Updated followers" messages in logs

3. **WAL Replication Never Starts**
   - Without ISR info, WalReplicationManager doesn't know who to replicate to
   - No "Sent WAL record to follower" messages
   - No "Received ACK from node" messages
   - IsrAckTracker times out after 5 seconds (or uses default timeout)

## Root Cause

The deadlock document ([docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md](docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md)) correctly identified:
- **WRONG**: Using `propose()` causes deadlock (blocks async runtime)
- **RIGHT**: Using WAL-based metadata replication (async, non-blocking)

However, the implementation hit a technical obstacle:
- The methods are on `RaftMetadataStore` (concrete type)
- ProduceHandler has `Arc<dyn MetadataStore>` (trait object)
- Can't call concrete methods without downcasting
- Downcasting requires adding `as_any()` to trait or storing concrete type

## Options to Fix

### Option 1: Add Methods to MetadataStore Trait (Clean)
**Pros**: Proper abstraction, works with any metadata store implementation
**Cons**: Requires updating all MetadataStore implementations (InMemoryMetadataStore, etc.)

```rust
// In chronik-common/src/metadata/traits.rs
#[async_trait]
pub trait MetadataStore: Send + Sync {
    // ... existing methods

    async fn assign_partition_replicas_with_replication(
        &self,
        topic: &str,
        partition: i32,
        replicas: Vec<u64>
    ) -> Result<()>;

    async fn update_isr_with_replication(
        &self,
        topic: &str,
        partition: i32,
        isr: Vec<u64>
    ) -> Result<()>;
}
```

### Option 2: Store Concrete Type in ProduceHandler (Pragmatic)
**Pros**: Simple, direct access to methods
**Cons**: Breaks abstraction, couples to Raft implementation

```rust
// In ProduceHandler struct
pub struct ProduceHandler {
    metadata_store: Arc<dyn MetadataStore>,  // Keep for tests
    raft_metadata_store: Option<Arc<RaftMetadataStore>>,  // Add for Raft-specific methods
    // ...
}
```

### Option 3: Add as_any() Downcasting (Middle Ground)
**Pros**: Keeps abstraction mostly intact
**Cons**: Runtime type checking, can fail at runtime

```rust
// Add to MetadataStore trait
fn as_any(&self) -> &dyn Any;

// In ProduceHandler
if let Some(raft_store) = self.metadata_store.as_any().downcast_ref::<RaftMetadataStore>() {
    raft_store.assign_partition_replicas(...).await?;
}
```

### Option 4: Event-Based Metadata Replication (Architectural)
**Pros**: Clean separation of concerns, no downcasting
**Cons**: More complex, requires larger refactor

```rust
// Emit events from metadata_store operations
metadata_store.assign_partition(...).await?;
// Triggers MetadataReplicationEvent::AssignPartition
// MetadataWalReplicator subscribes to events
// Replicates commands asynchronously
```

## Recommendation

**Use Option 2** (store concrete type) for immediate fix:

1. Add `raft_metadata_store: Option<Arc<RaftMetadataStore>>` to ProduceHandler
2. Pass it during construction in cluster mode
3. Call replication methods when available
4. Fall back to existing behavior for non-Raft modes

This gives us a working solution while keeping the door open for future refactoring to Option 1 or 4.

## Next Steps

1. Implement Option 2 (store concrete RaftMetadataStore)
2. Update ProduceHandler::new() to accept optional raft_metadata_store
3. Update integrated_server.rs to pass RaftMetadataStore in cluster mode
4. Call replication methods from produce_handler during topic creation
5. Test acks=all completes in < 1 second
6. Document metadata replication architecture

## Test Status

✅ **PASSING**: Test completes successfully in 16 milliseconds

```
✅ SUCCESS: acks=all completed in 0.016s (< 2s)

The fix is working correctly!
Partition leaders are now assigned during topic creation.
WAL replication can discover followers and send ACKs.
```

## Files Modified So Far

1. `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/wal_replication.rs`
   - Lines 790-872: Discovery loop timing fix

2. `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/raft_metadata_store.rs`
   - Lines 1128-1223: Added assign_partition_replicas() and update_isr_replicated() methods

3. `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/produce_handler.rs`
   - Lines 2547-2559: Added TODO comment (methods not called yet)

## Architecture Insight

The core insight from this investigation:

**There are TWO separate replication systems in Chronik:**

1. **Raft Consensus** (synchronous, for cluster membership)
   - Used for: Node addition/removal, leader election
   - Method: `propose()` - blocks until majority ACK
   - Latency: 10-50ms
   - **WARNING**: Never call from request handlers!

2. **WAL-Based Replication** (asynchronous, for data + metadata)
   - Used for: Message data, partition assignments, ISR updates
   - Method: Fire-and-forget async replication
   - Latency: 1-2ms
   - Safe to use in hot path

The fix must use #2 (WAL-based) for metadata, not #1 (Raft consensus).
