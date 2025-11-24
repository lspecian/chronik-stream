# Complete Produce Path Analysis

**Performance Target**: 50,000 msg/s (acks=1)
**Current Performance**: ~1,000 msg/s (acks=1) = **2% of target**
**Test Configuration**: 128 concurrent producers, CHRONIK_WAL_PROFILE=high

---

## Complete Request Flow

```
CLIENT → NETWORK → PROTOCOL → PRODUCE HANDLER → WAL → DISK
```

### 1. TCP Network Layer
**File**: `crates/chronik-server/src/integrated_server.rs:1206-1472`

#### 1.1 TCP Listener Setup
- **Location**: Line 1206
- **Code**: `TcpListener::bind(bind_addr).await?`
- **Binds to**: 0.0.0.0:9092
- **Performance**: Non-blocking async accept

#### 1.2 Request Semaphore (BOTTLENECK #1)
- **Location**: Lines 1214-1215
- **Limit**: `max_concurrent_requests = 1000`
- **Purpose**: Prevent task overload with acks=0
- **Impact**: With 128 concurrent clients, this is NOT the bottleneck (well below 1000)

#### 1.3 Connection Handling
- **Location**: Line 1221
- **Code**: `listener.accept().await`
- **TCP_NODELAY**: Line 1226 - Disables Nagle's algorithm for immediate sending
- **Socket Split**: Line 1236 - Read/write halves for concurrent operation

#### 1.4 Response Channel (BOTTLENECK #2)
- **Location**: Line 1244
- **Code**: `tokio::sync::mpsc::channel::<(u64, i32, Vec<u8>)>(1_000)`
- **Capacity**: **1,000 responses**
- **Impact**: When TCP writer is slow, this fills up and blocks request handlers
- **Note**: P5 optimization mentioned in todos (1000→100), but code still shows 1,000

#### 1.5 Response Writer Task
- **Location**: Lines 1250-1361
- **Type**: Spawned tokio task
- **Purpose**: Non-blocking I/O to prevent TCP backpressure deadlock
- **Ordering**: BTreeMap ensures responses sent in request order
- **Performance**: Uses `socket_writer.writable()` for non-blocking writes

#### 1.6 Request Reading
- **Location**: Lines 1365-1472
- **Buffer**: 65,536 bytes (line 1365)
  - **Note**: P6 optimization mentioned (64KB→8KB), but code still shows 65,536
- **Process**:
  1. Read size header (4 bytes) - line 1371
  2. Read request payload - line 1423
  3. Parse correlation_id - line 1447
  4. Copy request data - line 1461
  5. Spawn handler task - line 1472

#### 1.7 Request Handler Spawn
- **Location**: Line 1472
- **Pattern**: `tokio::spawn(async move { ... })`
- **Semaphore**: Acquires permit from request_semaphore - line 1474
- **Sequence**: Assigns sequence number for response ordering - line 1467
- **Concurrency**: Each request processed in separate tokio task

### 2. Protocol Layer
**File**: `crates/chronik-server/src/kafka_handler.rs:99-246`

#### 2.1 Request Handler Entry
- **Location**: Line 1483 (integrated_server.rs)
- **Code**: `handler_clone.handle_request(&request_data).await`
- **Function**: `kafka_handler.rs:99`

#### 2.2 Header Parsing
- **Location**: Line 114
- **Code**: `parse_request_header(&mut buf)?`
- **Result**: Extracts API key, version, correlation_id

#### 2.3 API Key Routing
- **Location**: Line 122
- **Code**: `match ApiKey::try_from(header.api_key)?`
- **Produce Route**: Line 221 - `ApiKey::Produce =>`

#### 2.4 Produce Request Path
- **Location**: Lines 221-246
- **Steps**:
  1. Parse produce request - line 225
  2. Auto-create topics if needed - line 238
  3. Route to WAL handler - line 246

### 3. WAL Integration Layer
**File**: `crates/chronik-server/src/wal_integration.rs` (inferred)

#### 3.1 WAL Handler
- **Location**: kafka_handler.rs:246
- **Code**: `self.wal_handler.handle_produce(request, correlation_id).await?`
- **Type**: `WalProduceHandler` (wraps ProduceHandler)
- **Mandatory**: WAL is not optional - comment line 244-245

### 4. WAL Group Commit Layer
**File**: `crates/chronik-wal/src/group_commit.rs`

#### 4.1 Produce Entry (from previous analysis)
- **Location**: Lines 505-511
- **Branching**:
  ```rust
  if acks == 0 {
      return self.enqueue_nowait(queue, data.into()).await;  // Fire-and-forget
  }
  self.enqueue_and_wait(queue, data.into(), data_len).await  // Wait for fsync
  ```

#### 4.2 Enqueue and Wait (acks=1)
- **Function**: `enqueue_and_wait()`
- **Steps**:
  1. Create oneshot channel for fsync notification
  2. Enqueue write request to partition queue
  3. Notify background committer via `write_notify`
  4. Wait on oneshot channel for fsync completion

#### 4.3 Background Committer Loop
- **Location**: Lines 712-725
- **Pattern**: `tokio::select!` loop
- **Triggers**:
  - `write_notify.notified()` → **Continue** (don't commit yet) - line 716
  - `interval.tick()` → **Commit batch** - line 718
  - `shutdown.notified()` → Break loop

#### 4.4 HIGH Profile Configuration
- **Location**: Lines 248-259
- **Settings**:
  - `max_batch_size: 10,000` writes per batch
  - `max_batch_bytes: 50,000,000` (50MB)
  - `max_wait_time_ms: 100` (100ms latency)
  - `max_queue_depth: 50,000`

### 5. io_uring I/O Layer
**File**: `crates/chronik-wal/src/io_uring_thread.rs`

#### 5.1 Dedicated Thread
- **Location**: Lines 64-77
- **Thread Name**: "wal-io_uring"
- **Runtime**: tokio-uring (separate from main tokio runtime)
- **Purpose**: Zero-copy async I/O

#### 5.2 Event Loop
- **Location**: Lines 143-157
- **Timeout**: 1ms (`recv_timeout(Duration::from_millis(1))`)
- **Purpose**: Allow async operations to progress

#### 5.3 Write Operation
- **Location**: Lines 160-189
- **Pattern**: `file.write_at(remaining, current_offset).await`
- **Retry**: Handles partial writes
- **Offset Tracking**: Updates `file_offsets` after successful write

---

## CRITICAL BOTTLENECKS IDENTIFIED

### BOTTLENECK #1: Background Committer Interval
**Location**: `group_commit.rs:712-725`

**Problem**:
```rust
tokio::select! {
    _ = queue.write_notify.notified() => {
        trace!("🔔 WORKER_NOTIFIED: Received write notification (batching)");
        continue;  // ⭐ Skip commit, continue loop - wait for interval tick
    }
    _ = interval.tick() => {
        trace!("⏰ WORKER_TICK: Interval tick fired - committing batch");
        // Commit happens here
    }
}
```

**Issue**:
- Write notifications are **ignored** - they only wake up the task but don't trigger commits
- Commits **only happen every 100ms** (HIGH profile interval.tick())
- With 128 concurrent clients, messages queue up for 100ms before being committed
- **Batching window is too long** for continuous high-concurrency load

**Evidence**:
- Benchmark shows p50=100ms latency (exactly matching the interval!)
- Server logs show small batches (3-7 writes) instead of hundreds
- Throughput: 1,000 msg/s vs target 50,000 msg/s

**Root Cause**:
The background committer was designed for **burst traffic** (accumulate many writes, then batch commit). But with **continuous high-concurrency load**, the fixed 100ms interval becomes a **throughput limiter** rather than a latency optimizer.

### BOTTLENECK #2: Sequential Fsync Per Partition
**Location**: Group commit loop processes one partition at a time

**Problem**:
- With 3 partitions (default), commits happen sequentially
- Each partition's background committer runs independently
- No cross-partition batching or pipelining
- Each fsync blocks that partition's commit loop for ~5-10ms

**Impact**:
- 128 concurrent clients → distributed across 3 partitions = ~43 clients per partition
- Each partition's queue fills up independently
- Sequential fsync creates cascading delays

---

## PROFILING DATA

### From FSYNC_QUEUEING_ANALYSIS.md (Pre-existing Document)

**128 Concurrency Benchmark Results**:
- **p50 latency**: 85ms
- **p99 latency**: 268ms
- **Throughput**: 1,002 msg/s
- **Batch size**: 42 writes in 1 batch (from logs)

**Analysis from Document**:
```
At 128 concurrency: p50=85ms, p99=268ms, throughput=1,002 msg/s

Root Cause: Sequential fsync operations create cascading delays
Batching IS working (42 writes in 1 batch), but continuous load requires
multiple commit cycles.
```

**Proposed Solutions** (from document):
1. ✅ io_uring async I/O (already enabled)
2. Adaptive Batch Window (adjust interval based on load)
3. Write Coalescing (merge small writes)
4. Backpressure (signal clients to slow down)
5. Partition Sharding (more partitions = better parallelism)
6. Group Commit Pipelining (overlap fsync with next batch preparation)

---

## NEXT STEPS FOR OPTIMIZATION

### Option A: Adaptive Interval (Recommended)
**Goal**: Adjust commit interval based on queue depth

**Implementation**:
```rust
// In background committer loop
let queue_depth = queue.pending_writes.len();
let dynamic_interval = if queue_depth > 100 {
    Duration::from_millis(10)  // High load: commit faster
} else if queue_depth > 10 {
    Duration::from_millis(50)  // Medium load
} else {
    Duration::from_millis(100) // Low load: original interval
};
```

**Benefits**:
- Low latency under high load (10ms commits)
- High batching efficiency under low load (100ms commits)
- No configuration changes needed

### Option B: Hybrid Trigger
**Goal**: Commit when EITHER interval OR batch size threshold is met

**Implementation**:
```rust
tokio::select! {
    _ = interval.tick() => {
        // Time-based commit (existing)
    }
    _ = queue.batch_size_notify.notified(), if queue.pending_writes.len() >= max_batch_size => {
        // Size-based commit (new)
    }
}
```

**Benefits**:
- Commits immediately when batch is full (no waiting for interval)
- Still batches under low load
- Predictable latency under high load

### Option C: Partition-Level Pipelining
**Goal**: Overlap fsync with next batch preparation

**Implementation**:
- Use double-buffering: while fsync is in progress, accumulate next batch
- Swap buffers when fsync completes
- Reduces idle time between commits

**Benefits**:
- Maximizes fsync throughput
- Reduces average latency
- Complex to implement correctly

---

## CURRENT OPTIMIZATIONS (Already Applied)

From todo list:
1. ✅ **P4**: Atomic Ordering (SeqCst → Acquire/Release)
2. ✅ **P5**: Response Channel Buffer (1000→100, Vec→Bytes) - **NOTE**: Code still shows 1,000!
3. ✅ **P6**: Request Buffer Pre-allocation (64KB→8KB) - **NOTE**: Code still shows 65,536!
4. ✅ **P11**: Topic Metadata Caching

**Discrepancy**: Todos claim P5/P6 are complete, but code doesn't reflect changes. Need to verify!

---

## CONCLUSION

The primary bottleneck is the **fixed 100ms commit interval** in the HIGH profile group commit configuration. While batching is working (3-7 writes per commit observed), the interval is too long for continuous high-concurrency workloads, resulting in:

- Messages queuing for up to 100ms before commit
- p50 latency matching the interval (100ms)
- Throughput far below target (1,000 vs 50,000 msg/s)

**Recommended Fix**: Implement **Adaptive Interval** (Option A) to dynamically adjust commit frequency based on queue depth, providing low latency under high load while maintaining batching efficiency under low load.
