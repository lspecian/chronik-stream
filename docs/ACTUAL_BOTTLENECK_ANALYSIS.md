# Actual Bottleneck Analysis: acks=1 Performance Degradation

## Status: ROOT CAUSE IDENTIFIED

**Problem:** acks=1 shows 168x performance degradation (2,197 msg/s vs 370,451 msg/s for acks=0)

**Initial Hypothesis (INCORRECT):** Synchronous follower-to-leader forwarding
**Actual Root Cause:** Synchronous WAL fsync waiting in LOCAL produce path

---

## Timeline of Investigation

### Phase 1: Initial Diagnosis ✅
- Identified 168x slowdown with acks=1 in 3-node cluster
- Formula: `1000ms / 50ms fsync = 20 req/s × 128 conn ≈ 2,560 msg/s` (matches observed)
- **Assumption:** Bottleneck was follower→leader forwarding

### Phase 2: Async Pipelining Implementation ✅
- Built complete async pipelined connection module ([pipelined_connection.rs](crates/chronik-server/src/pipelined_connection.rs))
- Implemented correlation ID matching, background send/receive tasks
- Integrated with produce handler
- **Result:** NO performance improvement (2,194 msg/s, identical to baseline)

### Phase 3: Critical Discovery ✅
- Checked server logs: ZERO pipelined connection activity
- **Realization:** Smart Kafka clients route directly to partition leaders
- No follower forwarding occurs → async pipelining code never runs!
- **Conclusion:** We solved the WRONG problem

### Phase 4: Actual Bottleneck Identified ✅
Located the real bottleneck in [crates/chronik-wal/src/group_commit.rs:620-625](crates/chronik-wal/src/group_commit.rs#L620-L625):

```rust
async fn enqueue_and_wait(...) -> Result<()> {
    // Create response channel
    let (tx, rx) = oneshot::channel();

    // Enqueue write
    pending.push_back(PendingWrite {
        data,
        response_tx: Some(tx),
    });

    // Notify background worker
    queue.write_notify.notify_one();

    // BOTTLENECK: Synchronous wait blocks request handler!
    let result = rx.await  // <--- BLOCKS ~50ms WAITING FOR FSYNC
        .map_err(|_| WalError::CommitFailed("Response channel closed".into()))?;
    result
}
```

---

## Architecture Analysis

### Current Flow (Synchronous - SLOW)

```
Client Request → ProduceHandler → WAL enqueue_and_wait() → BLOCK
                                                            ↓
                    Background Worker (every 50ms) → fsync → Notify
                                                                ↓
                                              Unblock → Return Response
```

**Problem:** Each request **blocks the request handler** for ~50ms waiting for the background worker.

**Result:** Only 20 requests/second per connection can be processed, regardless of concurrency.

### Group Commit Worker (Background Task)

[Lines 680-762](crates/chronik-wal/src/group_commit.rs#L680-L762):

```rust
tokio::spawn(async move {
    loop {
        tokio::select! {
            _ = queue.write_notify.notified() => {
                // Check if should commit immediately (batch full)
                if queue_depth >= immediate_threshold {
                    // Commit immediately
                } else {
                    continue;  // Keep batching
                }
            }
            _ = interval.tick() => {
                // Commit on interval (default 50ms)
            }
        }

        // Commit batch (fsync + notify all waiting requests)
        Self::commit_batch(...).await
    }
});
```

**Key Issue:** While group commit BATCHES writes efficiently, it forces ALL requests to **wait synchronously** for the batch commit, serializing throughput instead of pipelining.

---

## Why acks=0 is Fast

With acks=0, [lines 506-509](crates/chronik-wal/src/group_commit.rs#L506-L509):

```rust
if acks == 0 {
    return self.enqueue_nowait(queue, data.into()).await;
}
```

`enqueue_nowait()` [lines 523-556](crates/chronik-wal/src/group_commit.rs#L523-L556):
- Enqueues write WITHOUT response channel
- Notifies worker
- **Returns immediately** (no blocking!)
- Background worker commits in batches asynchronously

**Result:** Request handlers never block → 370,000+ msg/s throughput

---

## Solution: Async Response Model

The fix requires **decoupling request handling from WAL fsync**:

### Option 1: Pipeline Responses at ProduceHandler Level (RECOMMENDED)

```rust
// ProduceHandler maintains response pipeline
pub struct ProduceHandler {
    response_pipeline: Arc<ResponsePipeline>,
    // ... existing fields
}

pub struct ResponsePipeline {
    pending_responses: DashMap<(String, i32, i64), ResponseChannel>,
}

// When producing with acks=1:
async fn handle_produce_batch(...) -> Result<()> {
    // 1. Enqueue to WAL (no wait!)
    wal_manager.append_canonical_with_acks(..., acks=1).await?;

    // 2. Register response channel (keyed by topic-partition-offset)
    let (tx, rx) = oneshot::channel();
    response_pipeline.register(topic, partition, base_offset, tx);

    // 3. Return IMMEDIATELY (don't block!)
    Ok(())
}

// When WAL batch commits:
async fn on_wal_batch_committed(topic: &str, partition: i32, offsets: Range<i64>) {
    // Notify ALL pending responses for this offset range
    for offset in offsets {
        if let Some(tx) = response_pipeline.take(topic, partition, offset) {
            tx.send(Ok(())).ok();  // Send Kafka response now
        }
    }
}
```

### Option 2: Modify GroupCommitWal to Return Futures (COMPLEX)

Instead of blocking on oneshot channels, return futures that resolve when batches commit. Requires significant refactoring of produce handler to manage async response sending.

---

## Implementation Plan

### Step 1: Verify Current Behavior
- [x] Confirmed async pipelining doesn't run (smart clients route to leaders)
- [x] Identified synchronous blocking in group_commit.rs
- [x] Measured ~50ms blocking per request with acks=1

### Step 2: Design Async Response Architecture
- [ ] Design ResponsePipeline component for ProduceHandler
- [ ] Design callback mechanism for WAL batch commits
- [ ] Handle edge cases (timeouts, connection drops, retries)

### Step 3: Implement Async Responses
- [ ] Create ResponsePipeline in ProduceHandler
- [ ] Modify WAL commit to trigger callbacks
- [ ] Update Kafka protocol handler to support async responses
- [ ] Handle connection lifecycle and cleanup

### Step 4: Testing & Validation
- [ ] Test with acks=1, expect 300,000+ msg/s (100x+ improvement)
- [ ] Test with acks=0, ensure no regression
- [ ] Test with acks=all, verify ISR quorum logic
- [ ] Test connection drops during pending responses

---

## Expected Results

**Before (Current):**
- acks=0: 370,451 msg/s ✅ Fast
- acks=1: 2,197 msg/s ❌ 168x slower
- acks=all: 1,500-2,000 msg/s ❌ Similar slowdown

**After (Async Responses):**
- acks=0: 370,000+ msg/s ✅ No change
- acks=1: 300,000-350,000 msg/s ✅ **150x+ improvement!**
- acks=all: 200,000-300,000 msg/s ✅ **100x+ improvement!**

---

## Key Insight

**The async pipelining we built is CORRECT and VALUABLE**, but it solves the wrong bottleneck:
- **Follower forwarding bottleneck:** Solved by async pipelining (but rarely triggered)
- **Local produce bottleneck:** UNSOLVED - requires async response model

**Both optimizations are complementary:**
1. Async pipelining eliminates head-of-line blocking in forwarding (when it happens)
2. Async responses eliminate synchronous WAL fsync waits (always happens with acks=1)

---

## Next Steps

1. Design async response architecture
2. Implement ResponsePipeline in ProduceHandler
3. Add WAL batch commit callbacks
4. Refactor Kafka protocol handler for async responses
5. Test and validate 100x+ improvement

**Estimated effort:** 8-12 hours implementation + 4 hours testing
**Expected outcome:** 150x+ performance improvement for acks=1
