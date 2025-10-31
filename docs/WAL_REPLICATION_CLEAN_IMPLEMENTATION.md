# Clean WAL Replication Implementation Plan (v2.2.0)

**Target**: Implement PostgreSQL-style WAL streaming replication WITHOUT breaking v2.1.0's 78K msg/s performance

---

## Design Principles (NON-NEGOTIABLE)

1. **Zero locks in produce hot path** - All dependencies set during initialization
2. **Preserve v2.1.0 produce flow** - Keep pending_batches + flush logic intact
3. **Non-blocking replication** - Fire-and-forget async, never block produce
4. **Minimal changes** - Touch as little code as possible
5. **Test at each step** - Verify >= 75K msg/s after EVERY change

---

## Phase 1: Minimal Structure (THIS COMMIT)

**Goal**: Add replication field to ProduceHandler WITHOUT any functionality

**Changes**:
1. Add `WalReplicationManager` stub struct (empty for now)
2. Add field to `ProduceHandler`: `replication_manager: Option<Arc<WalReplicationManager>>`
3. Initialize as `None` in ProduceHandler::new()
4. Build and test - should maintain 78K msg/s (no logic changed)

**Success Criteria**:
- ✅ Code compiles cleanly
- ✅ Benchmark: >= 78K msg/s (no regression)
- ✅ No logic changes, just structural preparation

---

## Phase 2: Replication Hook (NEXT COMMIT)

**Goal**: Add replication call point WITHOUT actual implementation

**Changes**:
1. Add `replicate_async()` stub method to WalReplicationManager
2. Call after `flush_partition_if_needed()` in produce path
3. Make it instant return (tokio::spawn that does nothing)
4. Test - should still maintain 78K msg/s

**Success Criteria**:
- ✅ Hook in correct location (after flush)
- ✅ Non-blocking (tokio::spawn)
- ✅ Benchmark: >= 78K msg/s

---

## Phase 3: Actual Replication (CAREFUL COMMIT)

**Goal**: Implement actual WAL replication logic

**Changes**:
1. Implement WalReplicationManager with:
   - TCP connection pool to followers
   - Async send of WAL records
   - Error handling (log and continue, don't fail produce)
2. Wire up during server initialization (if replication configured)
3. Test standalone: >= 75K msg/s
4. Test with replication: >= 70K msg/s

**Success Criteria**:
- ✅ Standalone: >= 75K msg/s
- ✅ With replication: >= 70K msg/s
- ✅ Errors logged, don't crash produce
- ✅ Clean shutdown

---

## Architecture

### Data Flow (Preserves v2.1.0)

```
Producer Request
    ↓
ProduceHandler::process_produce_request()
    ↓
1. Assign offsets
2. Write to WAL (inline, with acks)              ← v2.1.0 path (preserve!)
3. Buffer to pending_batches                      ← v2.1.0 path (preserve!)
4. Update metrics
5. flush_partition_if_needed()                    ← v2.1.0 path (preserve!)
    ↓
6. REPLICATION HOOK (NEW, fire-and-forget):      ← v2.2.0 addition
   if let Some(ref repl_mgr) = self.replication_manager {
       tokio::spawn(async move {
           repl_mgr.replicate_async(...).await;
       });
   }
    ↓
7. Return response to producer                    ← v2.1.0 path (preserve!)
```

**Key**: Replication happens in Step 6, AFTER all v2.1.0 logic completes!

### WalReplicationManager Interface

```rust
pub struct WalReplicationManager {
    follower_connections: Arc<DashMap<String, TcpStream>>,
    send_queue: Arc<SegQueue<WalRecord>>,
}

impl WalReplicationManager {
    pub fn new(followers: Vec<String>) -> Self {
        // Initialize connection pool
    }

    pub async fn replicate_async(&self, record: WalRecord) {
        // Fire-and-forget: push to queue, background worker sends
        self.send_queue.push(record);
    }
}
```

**Critical**: Never blocks produce path!

---

## Testing Strategy

After EACH phase:

```bash
# 1. Build
cargo build --release --bin chronik-server

# 2. Start server
pkill -9 chronik-server
rm -rf ./data && mkdir -p ./data
RUST_LOG=info CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server --kafka-port 9092 --data-dir ./data standalone &

# 3. Benchmark
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic test-phase-N \
  --mode produce \
  --concurrency 128 \
  --duration 30s \
  --message-size 256 \
  --create-topic

# 4. Verify >= 75K msg/s (or STOP and revert)
```

---

## Failure Criteria (STOP IMMEDIATELY)

If ANY of these occur:
- ❌ Throughput < 50K msg/s
- ❌ p99 latency > 20ms
- ❌ Degradation over time (throughput drops during test)
- ❌ Server crashes or panics

Then:
1. REVERT the commit
2. Analyze what went wrong
3. Redesign before trying again

---

## Implementation Notes

### What NOT to Do (Learned from v2.2-v2.3 failure)

1. ❌ **Never wrap in RwLock**
   ```rust
   // WRONG (v2.3.0 mistake):
   replication_manager: Arc<RwLock<Option<Arc<WalReplicationManager>>>>

   // CORRECT (v2.2.0):
   replication_manager: Option<Arc<WalReplicationManager>>
   ```

2. ❌ **Never add early returns that skip critical logic**
   - v2.3.0 returned early, skipped pending_batches buffering
   - v2.2.0 runs ALL v2.1.0 logic, THEN optionally replicates

3. ❌ **Never block the produce path**
   - Replication must be tokio::spawn fire-and-forget
   - Errors logged, never fail produce request

### What TO Do

1. ✅ **Set dependencies during construction**
   ```rust
   impl ProduceHandler {
       pub fn new(..., replication_manager: Option<Arc<WalReplicationManager>>) -> Self {
           Self {
               replication_manager,  // Set once, never modified
               ...
           }
       }
   }
   ```

2. ✅ **Add logic at END of function, never early return**
   ```rust
   // After all v2.1.0 logic:
   if let Some(ref repl_mgr) = self.replication_manager {
       let record = record.clone();  // Avoid lifetime issues
       tokio::spawn(async move {
           if let Err(e) = repl_mgr.replicate_async(record).await {
               warn!("Replication failed: {:?}", e);  // Log, don't crash
           }
       });
   }
   ```

3. ✅ **Test incrementally**
   - Phase 1: Just structure → test
   - Phase 2: Hook location → test
   - Phase 3: Actual impl → test
   - Catch regressions early!

---

## Files to Modify

### Phase 1
- `crates/chronik-server/src/produce_handler.rs` - Add field
- `crates/chronik-server/src/lib.rs` - Add stub struct

### Phase 2
- `crates/chronik-server/src/produce_handler.rs` - Add hook call

### Phase 3
- Create `crates/chronik-replication/` crate (if needed)
- OR add to `crates/chronik-wal/src/replication.rs` (simpler)
- Wire up in `crates/chronik-server/src/main.rs`

---

## Success Metrics

**Phase 1** (Structure):
- Build time: < 2min
- Benchmark: 78K msg/s (no change)

**Phase 2** (Hook):
- Build time: < 2min
- Benchmark: 78K msg/s (no change)

**Phase 3** (Implementation):
- Standalone: >= 75K msg/s (within 5%)
- With replication: >= 70K msg/s (within 10%)
- Replication lag: < 100ms p99
- No crashes, clean shutdown

---

## Timeline

- Phase 1: 1 commit, 30min
- Phase 2: 1 commit, 30min
- Phase 3: 2-3 commits, 2-3 hours
- Total: ~4 hours for clean implementation

**vs v2.3.0**: Took days, resulted in 98.5% regression. Clean approach is faster AND safer!
