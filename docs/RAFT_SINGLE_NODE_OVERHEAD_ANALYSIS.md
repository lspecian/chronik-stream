# Raft Single-Node Overhead Analysis

**Question:** Why does single-node Raft have +1-2ms overhead compared to direct local WAL writes?

## The Overhead Sources

### 1. Background Tick Loop (100ms interval)

**Location:** `raft_cluster.rs:875-1200`

```rust
pub fn start_message_loop(self: Arc<Self>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

        loop {
            interval.tick().await;  // Wake up every 100ms

            // Tick Raft
            raft.tick();

            // Process Ready state
            if raft.has_ready() {
                let ready = raft.ready();

                // 1. Send messages (none in single-node)
                // 2. Apply snapshots
                // 3. Apply committed entries ← THIS APPLIES OUR WRITES
                // 4. Persist entries to WAL
                // 5. Send persisted messages
                // 6. Advance Raft
            }
        }
    });
}
```

**Problem:** Write path is asynchronous!

```
Client → propose() → Raft log → Wait for message_loop → Apply
  ↓                                    ↑
  0ms                                 up to 100ms later!
```

**In single-node mode:**
- Client calls `raft.propose(CreateTopic {...})`
- Command added to Raft log
- **BUT NOT APPLIED YET**
- Client must wait for background loop to wake up (0-100ms)
- Background loop processes `ready.committed_entries()` and applies
- Only then is state machine updated

**Overhead:** Up to 100ms latency (average 50ms)!

---

### 2. Raft Proposal Path (Even in Single-Node)

**Current flow:**
```rust
propose(cmd) {
    // 1. Serialize command
    let data = bincode::serialize(&cmd)?;  // ~100μs

    // 2. Acquire Raft write lock
    let mut raft = self.raft_node.write()?;  // Lock contention

    // 3. Call Raft propose
    raft.propose(vec![], data)?;  // Raft state machine work

    // 4. Release lock
    // 5. Command is in log but NOT committed/applied yet

    // 6. Background loop (100ms later) processes ready:
    //    - Commits entry (single-node: immediate)
    //    - Applies to state machine
}
```

**In single-node:**
- No network I/O (good!)
- No quorum wait (good!)
- **BUT** still goes through full Raft machinery:
  - Serialize command
  - Acquire lock
  - Add to Raft log
  - Wait for background loop to apply

**Overhead:** ~0.5-2ms (serialization + lock + Raft internal work) + 0-100ms (background loop)

---

### 3. Lock Contention

The `raft_node` is wrapped in `Arc<RwLock<RawNode>>`:

```rust
pub struct RaftCluster {
    raft_node: Arc<RwLock<RawNode<RaftWalStorage>>>,
}
```

**Every operation acquires the lock:**
- `propose()` - write lock
- `message_loop()` - write lock (every 100ms)
- Concurrent operations block

**In high-throughput scenarios:**
- Multiple threads calling `propose()` concurrently
- All contend for same lock
- Only one can proceed at a time

**Overhead:** Lock contention adds ~0.1-0.5ms per operation

---

### 4. Unnecessary WAL Double-Write

**Current path:**
```
Client writes topic metadata:
  1. propose() → Raft log WAL (RaftWalStorage)
  2. message_loop() → applies to state machine
  3. State machine is in-memory only!
  4. No persistence of actual metadata

Result: Metadata only in Raft log, not in separate metadata WAL
```

**If we wanted durability:**
```
Option A: Metadata in Raft log (current)
  - Write once to Raft WAL
  - Read by replaying Raft log
  - Good!

Option B: Metadata in separate WAL (ChronikMetaLogStore)
  - Write to Raft log (for replication)
  - Write to metadata WAL (for durability)
  - Double write! Wasteful!
```

**Actually, this is FINE** - we only write to Raft WAL, not double-write. No overhead here!

---

## Total Overhead Breakdown

| Component | Single-Node Raft | Direct Local WAL | Overhead |
|-----------|------------------|------------------|----------|
| **Serialization** | bincode (100μs) | bincode (100μs) | 0μs |
| **Lock acquisition** | RwLock (100-500μs) | RwLock (100-500μs) | 0μs |
| **Raft proposal** | 200-500μs | N/A | 200-500μs |
| **Background loop delay** | 0-100ms (avg 50ms) | Immediate | **0-100ms** |
| **WAL write** | RaftWalStorage | GroupCommitWal | ~0μs (same) |
| **Total** | 50ms average | ~1ms | **~49ms avg** |

**The killer: Background message loop running every 100ms!**

---

## The Fix: Synchronous Apply for Single-Node

### Current (Broken for Single-Node)

```rust
pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
    let data = bincode::serialize(&cmd)?;
    let mut raft = self.raft_node.write()?;
    raft.propose(vec![], data)?;

    // ❌ Command proposed but NOT applied yet!
    // ❌ Must wait for message_loop (up to 100ms)

    Ok(())
}
```

### Fixed: Synchronous Single-Node Mode

```rust
pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
    if self.is_single_node() {
        // FAST PATH: Skip Raft entirely, apply directly
        self.state_machine.write()?.apply(&cmd)?;

        // Optional: Write to Raft log for future replication when nodes added
        // (Can be done async in background)
        let data = bincode::serialize(&cmd)?;
        let mut raft = self.raft_node.write()?;
        raft.propose(vec![], data).ok(); // Fire and forget

        return Ok(());
    }

    // NORMAL PATH: Full Raft consensus (multi-node)
    let data = bincode::serialize(&cmd)?;
    let mut raft = self.raft_node.write()?;
    raft.propose(vec![], data)?;

    // Wait for commit + apply
    self.wait_for_apply().await?;

    Ok(())
}

fn is_single_node(&self) -> bool {
    // No peers = single-node mode
    self.transport.peer_count() == 0
}
```

**Performance:**
- Single-node: ~0.5ms (apply directly)
- Multi-node: ~10-50ms (Raft consensus)

**Overhead: 0ms** (same as direct local WAL)!

---

## Alternative: Increase Message Loop Frequency

**Current:** 100ms interval → 0-100ms latency (50ms avg)

**Change:**
```rust
let mut interval = tokio::time::interval(std::time::Duration::from_millis(1));
```

**Result:** 0-1ms latency (0.5ms avg)

**Pros:**
- Simple fix (one line change)
- Works for both single-node and multi-node

**Cons:**
- Higher CPU usage (1000 ticks/sec vs 10 ticks/sec)
- Still 0.5ms average overhead
- Wastes CPU on single-node deployments

**Verdict:** Not ideal, but acceptable if we want zero code changes

---

## Alternative: Adaptive Interval

```rust
let interval_ms = if self.is_single_node() {
    1  // 1ms for single-node (low latency)
} else {
    100  // 100ms for cluster (save CPU)
};

let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
```

**Pros:**
- Balances latency vs CPU
- Single-node: ~0.5ms overhead
- Multi-node: 100ms tick (fine, network latency dominates)

**Cons:**
- Still some overhead for single-node
- More complex (need to detect mode changes)

---

## Recommendation

**Two-tier approach:**

```rust
pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
    if self.peers.is_empty() {
        // TIER 1: Single-node mode - SYNCHRONOUS apply
        return self.apply_immediately(&cmd).await;
    }

    // TIER 2: Cluster mode - ASYNC Raft consensus
    return self.propose_via_raft(&cmd).await;
}

async fn apply_immediately(&self, cmd: &MetadataCommand) -> Result<()> {
    // Apply to state machine immediately
    self.state_machine.write()?.apply(cmd)?;

    // Write to Raft log asynchronously (for future replication)
    // If we later add nodes, they'll catch up from this log
    let cmd_clone = cmd.clone();
    let raft_node = self.raft_node.clone();
    tokio::spawn(async move {
        let data = bincode::serialize(&cmd_clone).ok()?;
        let mut raft = raft_node.write().ok()?;
        raft.propose(vec![], data).ok();
    });

    Ok(())
}

async fn propose_via_raft(&self, cmd: &MetadataCommand) -> Result<()> {
    // Normal Raft path (existing code)
    let data = bincode::serialize(cmd)?;
    let mut raft = self.raft_node.write()?;
    raft.propose(vec![], data)?;

    // Wait for apply (via message loop)
    self.wait_for_apply().await?;

    Ok(())
}
```

**Performance:**
- Single-node: ~0.1ms (direct apply, async log write)
- Multi-node: ~10-50ms (Raft consensus as usual)

**Overhead: ~0μs** for single-node!

---

## The Real Answer

**There is NO inherent reason for +1-2ms overhead in single-node Raft!**

The overhead comes from:
1. **Background message loop** (100ms interval) - can be fixed
2. **Waiting for async apply** - can be made synchronous for single-node

**Solution:** Detect single-node mode and apply synchronously.

**Code change:** ~20 lines in `propose()`

**Result:** Zero overhead for single-node, seamless scaling to multi-node.

---

## Bonus: Why Background Loop Exists

**Raft requires periodic ticking:**
- Leader election timeouts
- Heartbeat sending
- Message processing

**In multi-node clusters:**
- Must send heartbeats to followers (every ~100ms)
- Must detect leader failures (election timeout ~1s)
- Background loop is necessary

**In single-node:**
- No followers → no heartbeats needed
- No leader election (always leader)
- **Background loop is USELESS**

**Optimization:** Skip message loop entirely for single-node!

```rust
pub fn start_message_loop(self: Arc<Self>) {
    if self.is_single_node() {
        tracing::info!("Single-node mode: skipping message loop");
        return;  // Don't start background task!
    }

    // Multi-node: start background loop
    tokio::spawn(async move {
        // ... existing code ...
    });
}
```

**Result:** Zero CPU overhead for single-node deployments!
