# Fsync Queueing Analysis: GroupCommitWal Implementation

## Executive Summary

**Status:** ✅ IMPLEMENTED AND WORKING (v1.3.52+)

Chronik Stream uses a **PostgreSQL-style group commit** implementation in [crates/chronik-wal/src/group_commit.rs](crates/chronik-wal/src/group_commit.rs) that achieves **100,000+ writes/sec per partition** with **~50ms p99 latency** under load.

### Key Performance Characteristics

| Metric | Value | Notes |
|--------|-------|-------|
| **Throughput** | 100,000+ writes/sec per partition | Measured with acks=0 |
| **Latency (p99)** | ~50ms | Under load, matches commit interval |
| **Latency (p50)** | ~10-25ms | Average batch wait time |
| **Batching** | Up to 1000 writes per fsync | Default `max_batch_size` |
| **Fsync interval** | 50ms (high profile) | Tunable via `CHRONIK_WAL_PROFILE` |

---

## Problem: acks=1 Performance Degradation (v2.2.8)

**Observed Issue:**
- acks=0: 370,451 msg/s ✅ Fast
- acks=1: 2,197 msg/s ❌ **168x slower!**

**Root Cause:** Synchronous blocking on WAL fsync in [group_commit.rs:622](crates/chronik-wal/src/group_commit.rs#L622):

```rust
async fn enqueue_and_wait(...) -> Result<()> {
    let (tx, rx) = oneshot::channel();

    pending.push_back(PendingWrite {
        data,
        response_tx: Some(tx),
    });

    queue.write_notify.notify_one();

    // BOTTLENECK: Blocks request handler for ~50ms!
    let result = rx.await?;  // <--- WAITS FOR FSYNC
    result
}
```

**Why this is slow:**
- Background worker commits every 50ms (max_wait_time_ms)
- Each request blocks for full fsync duration
- Result: Only 20 requests/second per connection (1000ms / 50ms)
- Formula: 20 req/s × 128 connections ≈ 2,560 msg/s ✅ Matches observed

---

## Solution: Async Response Pipeline (v2.2.10)

### Architecture

**Current Flow (SLOW - blocks on fsync):**
```
Client → ProduceHandler → WAL.enqueue_and_wait() → BLOCK 50ms → Return Response
```

**New Flow (FAST - async responses):**
```
Client → ProduceHandler → WAL.enqueue(acks=0)  → Register in ResponsePipeline → Return IMMEDIATELY
    ↓
GroupCommit Worker (every 50ms) → fsync batch → Trigger callback → ResponsePipeline → Send Responses
```

### Implementation Plan

#### Phase 1: ResponsePipeline Component ✅ DONE

Created [response_pipeline.rs](crates/chronik-server/src/response_pipeline.rs) with:
- DashMap-based pending response tracking
- Registration by (topic, partition, base_offset)
- Batch notification when WAL commits
- Background timeout cleanup
- Metrics collection

#### Phase 2: GroupCommitWal Callback Mechanism (IN PROGRESS)

Modify [group_commit.rs](crates/chronik-wal/src/group_commit.rs):

1. **Add metadata to PendingWrite:**
```rust
struct PendingWrite {
    data: Bytes,
    response_tx: Option<oneshot::Sender<Result<()>>>,

    // NEW: Metadata for async response callbacks
    topic: String,
    partition: i32,
    base_offset: i64,
    last_offset: i64,
}
```

2. **Add callback field to GroupCommitWal:**
```rust
pub type CommitCallback = Arc<dyn Fn(&str, i32, i64, i64) + Send + Sync>;

pub struct GroupCommitWal {
    // ... existing fields

    /// Optional callback invoked after successful batch commit
    /// Args: (topic, partition, min_offset, max_offset)
    commit_callback: Option<CommitCallback>,
}
```

3. **Invoke callback in commit_batch():**
```rust
async fn commit_batch(...) -> Result<()> {
    // ... existing fsync logic ...

    // Extract metadata from batch
    let topic = &queue.topic;
    let partition = queue.partition;
    let min_offset = batch.iter().map(|w| w.base_offset).min().unwrap();
    let max_offset = batch.iter().map(|w| w.last_offset).max().unwrap();

    // Invoke callback AFTER successful fsync
    if let Some(callback) = &self.commit_callback {
        callback(topic, partition, min_offset, max_offset);
    }

    // Notify waiters (existing oneshot channels)
    for write in batch {
        if let Some(tx) = write.response_tx {
            let _ = tx.send(Ok(()));
        }
    }

    Ok(())
}
```

#### Phase 3: ProduceHandler Integration

Modify [produce_handler.rs](crates/chronik-server/src/produce_handler.rs):

```rust
pub struct ProduceHandler {
    // ... existing fields
    response_pipeline: Arc<ResponsePipeline>,
}

async fn handle_produce_batch(...) -> Result<()> {
    // 1. Write to WAL with acks=0 (fire-and-forget, no blocking!)
    wal_manager.append_canonical_with_acks(..., acks=0).await?;

    // 2. If acks=1 or acks=-1, register for async response
    if acks != 0 {
        let (tx, rx) = oneshot::channel();
        response_pipeline.register(topic, partition, base_offset, last_offset, tx);

        // Wait for async callback (non-blocking on WAL!)
        let response = rx.await?;
        return Ok(response);
    }

    // 3. If acks=0, return immediately
    Ok(default_response())
}
```

#### Phase 4: Wire GroupCommitWal Callback

In [wal_integration.rs](crates/chronik-server/src/wal_integration.rs) or ProduceHandler init:

```rust
// Create ResponsePipeline
let response_pipeline = Arc::new(ResponsePipeline::new());

// Create callback closure
let pipeline_clone = response_pipeline.clone();
let callback: CommitCallback = Arc::new(move |topic, partition, min_offset, max_offset| {
    pipeline_clone.notify_batch_committed(topic, partition, min_offset, max_offset);
});

// Create GroupCommitWal with callback
let wal = GroupCommitWal::with_callback(config, callback);
```

---

## Expected Performance

**Before (v2.2.9):**
- acks=0: 370,451 msg/s ✅ Fast
- acks=1: 2,197 msg/s ❌ 168x slower
- acks=all: 1,500-2,000 msg/s ❌ Similar slowdown

**After (v2.2.10 - async responses):**
- acks=0: 370,000+ msg/s ✅ No change
- acks=1: **300,000-350,000 msg/s** ✅ **150x+ improvement!**
- acks=all: **200,000-300,000 msg/s** ✅ **100x+ improvement!**

---

## Testing Plan

```bash
# Build with async responses
cargo build --release --bin chronik-server

# Start cluster
./tests/cluster/start.sh

# Test acks=0 (baseline)
./target/release/chronik-bench -c 128 -s 256 -d 30s --acks 0 \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094

# Test acks=1 (expect 300K+ msg/s!)
./target/release/chronik-bench -c 128 -s 256 -d 30s --acks 1 \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094

# Test acks=all
./target/release/chronik-bench -c 128 -s 256 -d 30s --acks all \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094
```

---

## Implementation Status

- [x] Phase 1: ResponsePipeline component created
- [ ] Phase 2: Group CommitWal callback mechanism
- [ ] Phase 3: ProduceHandler integration
- [ ] Phase 4: End-to-end testing

**Target Completion:** Next 2-3 hours

---

## Key Insights

1. **PostgreSQL-style group commit is EXCELLENT** for throughput, but creates latency for synchronous waiters
2. **Async response delivery** decouples request handling from fsync completion
3. **Both optimizations are valuable:**
   - Async pipelining (v2.2.9): Eliminates follower forwarding head-of-line blocking
   - Async responses (v2.2.10): Eliminates local WAL fsync blocking

4. **The 168x bottleneck is NOT in the WAL itself** - group commit batching works perfectly. The problem is forcing callers to wait synchronously for batches.

5. **Fire-and-forget WAL writes** + **async response callbacks** = Best of both worlds!
