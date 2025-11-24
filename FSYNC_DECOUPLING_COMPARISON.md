# Fsync Decoupling: Kafka vs Redpanda vs PostgreSQL

## Executive Summary

This document analyzes how three high-performance systems decouple client responses from fsync completion to achieve high throughput while maintaining durability guarantees. The key architectural differences lie in:

1. **When acknowledgments are sent** (before vs after fsync)
2. **How batching happens** (client-side pipelining vs server-side grouping)
3. **What durability guarantees are actually provided**

---

## 1. Kafka (acks=1): Client-Side Pipelining + Delayed Fsync

### Architecture Overview

Kafka achieves high throughput by **sending acks BEFORE fsync** and relying on **client-side pipelining** to maintain a queue of inflight requests.

### Key Mechanism: max.in.flight.requests.per.connection

**Purpose**: Allows producer to send multiple produce requests without waiting for each acknowledgment.

**How it works**:
```
Producer Side (Client Pipelining):
┌─────────────────────────────────────────────────────────┐
│  Producer Buffer (RecordAccumulator)                    │
│  ├─ Batch 1 (topic-partition-1) [SENDING]              │
│  ├─ Batch 2 (topic-partition-1) [SENDING]              │
│  ├─ Batch 3 (topic-partition-2) [SENDING]              │
│  ├─ Batch 4 (topic-partition-1) [QUEUED]               │
│  └─ Batch 5 (topic-partition-2) [QUEUED]               │
│                                                          │
│  max.in.flight.requests.per.connection = 5              │
│  → Can have 5 concurrent unacked requests per broker    │
└─────────────────────────────────────────────────────────┘
         ↓ ↓ ↓ (multiple concurrent TCP requests)
┌─────────────────────────────────────────────────────────┐
│  Broker (Leader)                                         │
│  ├─ Receive Batch 1 → Append to LogSegment             │
│  ├─ Send ACK 1 ✓ (BEFORE FSYNC!)                       │
│  ├─ Receive Batch 2 → Append to LogSegment             │
│  ├─ Send ACK 2 ✓ (BEFORE FSYNC!)                       │
│  └─ Receive Batch 3 → Append to LogSegment             │
│     Send ACK 3 ✓ (BEFORE FSYNC!)                       │
│                                                          │
│  Background Thread:                                      │
│    - Every log.flush.interval.ms (default: not set)     │
│    - Or log.flush.scheduler.interval.ms (default: none) │
│    → fsync() all pending writes                         │
└─────────────────────────────────────────────────────────┘
```

### When Acks Are Sent (acks=1)

**Critical Timing**: Kafka broker sends ACK immediately after:
1. Appending record batch to page cache (memory-mapped file or FileChannel buffer)
2. Updating offset and timestamp indexes
3. **WITHOUT waiting for fsync to disk**

**Source Code Flow** (conceptual):
```java
// Kafka LogSegment.append()
def append(records: MemoryRecords): Unit = {
  // 1. Write to FileChannel (page cache, NOT disk!)
  fileChannel.write(records.buffer())

  // 2. Update indexes
  offsetIndex.append(offset, physicalPosition)
  timeIndex.append(timestamp, offset)

  // 3. Return immediately - ACK sent to producer
  // fsync happens LATER by background thread!
}

// Separate background thread or periodic scheduler:
def flush(): Unit = {
  fileChannel.force(true)  // This is fsync - happens async!
}
```

### Broker-Side Batching (Group Commit)

**Does Kafka do server-side group commit?**

**YES, but implicitly** - Not through explicit producer request batching, but through:

1. **Background Flush Thread**: `log.flush.interval.ms` batches all pending writes
2. **Natural Accumulation**: Multiple producers writing concurrently → all fsynced together
3. **NO explicit wait queue** for producers (they already got their ACKs!)

```
Timeline:
T=0ms:    Producer A sends batch → Broker appends → ACK sent (no fsync yet)
T=5ms:    Producer B sends batch → Broker appends → ACK sent (no fsync yet)
T=10ms:   Producer C sends batch → Broker appends → ACK sent (no fsync yet)
T=50ms:   Background flush thread wakes up
          → fsync() all 3 batches at once (GROUP COMMIT!)
          → Producers don't wait for this!
```

### Durability Guarantee (acks=1)

**What you get**:
- ✅ Data is in leader's page cache (survives process crash if OS flushes)
- ✅ Data is replicated to followers asynchronously (eventual durability)
- ❌ **NOT guaranteed on disk** at ack time
- ❌ Risk: Power failure before background fsync = data loss

**Kafka's philosophy**: Rely on replication, not fsync, for durability. OS page cache is "good enough" for acks=1.

### Configuration Parameters

```properties
# Producer-side (client pipelining)
max.in.flight.requests.per.connection=5  # Default: very high (1000000 in some versions)
batch.size=16384                          # Batch size in bytes
linger.ms=0                               # Wait time to accumulate batches

# Broker-side (background fsync)
log.flush.interval.messages=Long.MaxValue  # Default: don't force flush by count
log.flush.interval.ms=None                 # Default: rely on OS page cache
log.flush.scheduler.interval.ms=None       # Default: no periodic flush

# For stronger durability (not typical):
log.flush.interval.messages=1              # Flush every message (kills throughput!)
log.flush.interval.ms=1000                 # Flush every second
```

### Performance Characteristics

- **Latency (acks=1)**: 1-5ms typical (network + page cache write)
- **Throughput**: Very high (no fsync bottleneck)
- **Durability**: Weak at ack time, strong eventually (replication + OS flush)

---

## 2. Redpanda (acks=1): Async Fsync with io_uring

### Architecture Overview

Redpanda uses **io_uring** for async fsync, achieving **BOTH high throughput AND stronger durability** compared to Kafka.

### Key Mechanism: io_uring Async Fsync

**Critical Difference from Kafka**: Redpanda **DOES fsync before ack** (even with acks=1), but makes it **non-blocking** via io_uring.

```
Producer → Redpanda Leader (Seastar Reactor)
┌─────────────────────────────────────────────────────────┐
│  Seastar Reactor Thread (C++, no JVM garbage collection)│
│                                                          │
│  1. Receive produce request                             │
│  2. Append to Raft log (memory buffer)                  │
│  3. Submit fsync via io_uring (non-blocking!)           │
│     ├─ io_uring_prep_fsync(&sqe, fd)                   │
│     └─ io_uring_submit(&ring)  ← Returns immediately    │
│                                                          │
│  4. Reactor continues processing other requests         │
│     (no blocking! CPU does other work)                  │
│                                                          │
│  5. io_uring completion event fires (fsync done)        │
│     └─ Send ACK to producer ✓                          │
│                                                          │
│  Timeline for single request:                           │
│    - Submit fsync: ~5μs (syscall overhead)              │
│    - Wait for disk: 1-5ms (async, no CPU blocking)      │
│    - ACK sent: After fsync completes                    │
└─────────────────────────────────────────────────────────┘
```

### When Acks Are Sent (acks=1)

**Redpanda Timing**: ACK sent **AFTER fsync completes**, but:
- fsync is **non-blocking** (io_uring async I/O)
- CPU continues processing other requests during disk wait
- Multiple fsync operations can be in-flight concurrently

**Pseudocode**:
```cpp
// Redpanda append (simplified)
future<void> append_batch(record_batch batch) {
  // 1. Append to Raft log buffer
  raft_log.append(batch);

  // 2. Submit async fsync (non-blocking!)
  co_await disk_io.fsync(log_fd);  // io_uring internally

  // 3. ACK only after fsync completes
  // But no thread blocking - Seastar reactor handles it
}
```

### Server-Side Batching

**Implicit batching via io_uring**:

```
Timeline:
T=0ms:    Request A arrives → Submit fsync_A to io_uring queue
T=1ms:    Request B arrives → Submit fsync_B to io_uring queue
T=2ms:    Request C arrives → Submit fsync_C to io_uring queue
T=5ms:    Kernel batches all 3 fsync ops into single disk operation
          → All 3 complete together (GROUP COMMIT at kernel level!)
T=6ms:    io_uring completion events fire
          → ACK A, ACK B, ACK C sent to producers
```

**Key insight**: io_uring kernel does the batching, not Redpanda userspace code!

### Durability Guarantee (acks=1)

**What you get**:
- ✅ **Data IS on disk** when ACK is sent
- ✅ No risk from power failure (already fsync'd)
- ✅ Lower latency than traditional blocking fsync (async I/O)
- ✅ Raft consensus ensures consistency

**Redpanda's philosophy**: Durability by default, performance via async I/O.

### Configuration Parameters

```yaml
# Redpanda config (redpanda.yaml)
# Note: acks=1 DOES fsync in Redpanda (unlike Kafka)

# Async I/O settings
storage:
  enable_io_uring: true              # Use io_uring (Linux 5.1+)
  io_queue_depth: 128                # Concurrent I/O operations

# Raft log settings
raft:
  group_commit_bytes: 1MB            # Batch threshold
  group_commit_timeout_ms: 5ms       # Max wait for batching
```

### Performance Characteristics

- **Latency (acks=1)**: 2-10ms (includes fsync, but async)
- **Throughput**: Very high (io_uring parallelism + zero-copy)
- **Durability**: Strong (fsync before ack)

### Why Redpanda is Faster Than Kafka Despite Fsync

1. **No JVM overhead** (C++ vs Java)
2. **io_uring** batches disk ops at kernel level
3. **Seastar reactor** keeps CPU busy during disk waits
4. **Zero-copy** data paths (DMA directly to NVMe)

---

## 3. PostgreSQL: Server-Side Group Commit with Wait Queue

### Architecture Overview

PostgreSQL uses **explicit server-side group commit** - multiple transactions wait in a queue until a single fsync flushes all their WAL records together.

### Key Mechanism: commit_delay + commit_siblings

**Purpose**: Batch multiple concurrent transactions into one fsync.

```
Client Connections (No Pipelining Required):
┌─────────────────────────────────────────────────────────┐
│  Client 1: BEGIN; INSERT ...; COMMIT;  [WAITING]        │
│  Client 2: BEGIN; UPDATE ...; COMMIT;  [WAITING]        │
│  Client 3: BEGIN; DELETE ...; COMMIT;  [WAITING]        │
└─────────────────────────────────────────────────────────┘
         ↓ ↓ ↓ (each sends COMMIT command)
┌─────────────────────────────────────────────────────────┐
│  PostgreSQL Backend Processes                            │
│                                                          │
│  Backend 1:                                             │
│    XLogInsert() → Write WAL record to shared buffer     │
│    XLogFlush() → Request flush to LSN_1                 │
│      ├─ Check: commit_siblings >= threshold?            │
│      ├─ YES: sleep(commit_delay) [10μs-10ms]            │
│      └─ Wait in queue for flush...                      │
│                                                          │
│  Backend 2:                                             │
│    XLogInsert() → Write WAL record to shared buffer     │
│    XLogFlush() → Request flush to LSN_2                 │
│      └─ Join same wait queue (LSN_2 > LSN_1)            │
│                                                          │
│  Backend 3:                                             │
│    XLogInsert() → Write WAL record to shared buffer     │
│    XLogFlush() → Request flush to LSN_3                 │
│      └─ Join same wait queue (LSN_3 > LSN_2)            │
│                                                          │
│  WAL Writer (after commit_delay expires):               │
│    write() all WAL records up to max LSN (LSN_3)        │
│    fsync() WAL file ← ONE FSYNC FOR ALL 3 TXNS!        │
│    Wake all waiting backends (LSN <= LSN_3)             │
│                                                          │
│  Backend 1, 2, 3: Send COMMIT ACK to clients ✓         │
└─────────────────────────────────────────────────────────┘
```

### When Acks Are Sent

**PostgreSQL Timing**: ACK sent **AFTER fsync completes**.

**Timeline**:
```
T=0ms:    Client 1 sends COMMIT → Backend writes WAL → Enters wait queue
T=1ms:    Client 2 sends COMMIT → Backend writes WAL → Enters wait queue
T=2ms:    Client 3 sends COMMIT → Backend writes WAL → Enters wait queue
T=2ms+ε:  commit_delay expires (e.g., 10μs if configured)
          → WAL writer wakes up
          → fsync() all 3 transactions' WAL records
T=5ms:    fsync completes → Wake all 3 backends
          → Send COMMIT ACKs to all 3 clients ✓
```

### Server-Side Batching (Pure Group Commit)

**Key Parameters**:

```sql
-- postgresql.conf
commit_delay = 10                  -- Microseconds to wait (0-100000)
commit_siblings = 5                -- Minimum concurrent txns to trigger delay

-- How it works:
-- If < 5 concurrent transactions: commit_delay = 0 (no wait)
-- If >= 5 concurrent transactions: wait up to 10μs before fsync
```

**Decision Logic**:
```c
// Pseudocode from PostgreSQL XLogFlush
void XLogFlush(XLogRecPtr lsn) {
  // 1. Check if delay is worthwhile
  int concurrent_txns = count_active_backends();

  if (concurrent_txns >= commit_siblings && commit_delay > 0) {
    // 2. Sleep briefly to let more txns join
    pg_usleep(commit_delay);
  }

  // 3. Acquire WAL write lock (first backend to get here does the work)
  if (XLogCtl->lastFlushedLSN >= lsn) {
    // Another backend already flushed our WAL
    return;
  }

  // 4. Write and fsync WAL (covers ALL waiting backends)
  write(wal_fd, wal_buffer, size);
  fsync(wal_fd);  // ← ONE FSYNC FOR MANY TXNS

  // 5. Update lastFlushedLSN and wake all waiters
  XLogCtl->lastFlushedLSN = max_lsn_in_buffer;
  wakeup_all_waiting_backends();
}
```

### Client-Side: No Pipelining Required!

**Critical difference from Kafka**: PostgreSQL group commit is **purely server-side**.

- Clients send one COMMIT at a time (blocking, waiting for ACK)
- Server batches them together internally
- No need for client to send multiple concurrent requests

```python
# PostgreSQL client (blocking commits)
conn1.execute("BEGIN")
conn1.execute("INSERT ...")
conn1.execute("COMMIT")  # Blocks until fsync completes
# ↑ ACK includes fsync durability guarantee

# Kafka client (pipelined produces)
producer.send("topic", msg1)  # Returns immediately (async)
producer.send("topic", msg2)  # Returns immediately (async)
producer.send("topic", msg3)  # Returns immediately (async)
# ↑ max.in.flight.requests allows 5 concurrent unacked requests
# ↑ ACKs do NOT include fsync guarantee
```

### Durability Guarantee

**What you get**:
- ✅ **Data IS on disk** when COMMIT ACK is sent
- ✅ ACID durability guaranteed
- ✅ No data loss on power failure
- ✅ WAL guarantees crash recovery

**PostgreSQL's philosophy**: Durability is non-negotiable, optimize with group commit.

### Configuration Parameters

```sql
-- Aggressive group commit (high throughput, slight latency increase)
commit_delay = 100                      -- 100μs wait
commit_siblings = 3                     -- Need 3+ concurrent txns

-- No group commit delay (low latency, lower throughput)
commit_delay = 0                        -- No wait (default)
commit_siblings = 5                     -- Threshold doesn't matter

-- Alternative: Async commit (like Kafka acks=1)
synchronous_commit = off                -- Don't wait for fsync (risk data loss!)
```

### Performance Characteristics

- **Latency (sync commit)**: 5-20ms (includes fsync wait)
- **Throughput**: High under concurrency (group commit batching)
- **Durability**: Strong (fsync before ack, unless async commit)

---

## Architectural Comparison Table

| Aspect | Kafka (acks=1) | Redpanda (acks=1) | PostgreSQL (sync commit) |
|--------|----------------|-------------------|--------------------------|
| **When ACK sent** | BEFORE fsync | AFTER fsync | AFTER fsync |
| **Fsync timing** | Background thread (delayed) | Immediate (async via io_uring) | Immediate (blocking or grouped) |
| **Batching mechanism** | Client-side pipelining | Kernel-level io_uring batching | Server-side wait queue |
| **Client must pipeline?** | YES (max.in.flight.requests) | Optional (helps but not required) | NO (server batches internally) |
| **Durability at ACK time** | Weak (page cache only) | Strong (on disk) | Strong (on disk) |
| **Throughput strategy** | Decouple ACK from fsync | Async fsync (non-blocking) | Group commit (batch fsync) |
| **CPU during fsync** | Available (async background) | Available (io_uring async) | Blocked (unless group commit) |
| **Risk of data loss** | YES (power failure before flush) | NO (fsync before ACK) | NO (fsync before ACK) |
| **Typical latency** | 1-5ms | 2-10ms | 5-20ms |
| **Configuration complexity** | Medium (producer + broker) | Low (works out of box) | Low (optional tuning) |

---

## Trade-off Analysis

### Kafka: Maximum Throughput, Weakest Durability (acks=1)

**Pros**:
- Lowest latency (no fsync wait)
- Highest throughput (no fsync bottleneck)
- Client-side pipelining maximizes network utilization

**Cons**:
- Data loss risk on power failure
- Must use acks=all for true durability (much slower)
- Requires client configuration (max.in.flight.requests)

**Best for**: High-volume log aggregation where some data loss is acceptable.

### Redpanda: Best of Both Worlds (Modern I/O)

**Pros**:
- Strong durability (fsync before ack)
- High throughput (io_uring async I/O)
- No client-side complexity (works like Kafka)

**Cons**:
- Requires modern kernel (Linux 5.1+)
- Slightly higher latency than Kafka acks=1
- C++ codebase (less familiar to Java ecosystem)

**Best for**: Modern deployments needing both speed and durability.

### PostgreSQL: ACID Guarantees, Server-Side Simplicity

**Pros**:
- Strong durability (ACID compliance)
- No client pipelining needed (simple client code)
- Group commit works automatically under load

**Cons**:
- Higher latency than async systems
- Throughput limited by single WAL file
- Not designed for Kafka-style streaming workloads

**Best for**: Traditional transactional workloads (OLTP).

---

## Key Insights for Chronik

### 1. Kafka's Approach is NOT "Group Commit"

Kafka does NOT batch producer requests server-side. Instead:
- **Client pipelines** multiple requests (max.in.flight.requests)
- **Server acks immediately** (no fsync wait)
- **Background thread** fsyncs later (batches all pending writes)

**This is decoupling, not grouping!**

### 2. True Group Commit Requires Server-Side Wait Queue

PostgreSQL-style group commit:
- Backends **wait in a queue** after writing WAL
- First backend to acquire flush lock **fsyncs for everyone**
- All waiting backends wake up and send ACKs together

**This requires blocking the response path!**

### 3. io_uring Enables "Async Group Commit"

Redpanda achieves both durability and throughput by:
- Submitting fsync immediately (not delayed)
- Using io_uring to make fsync non-blocking
- Kernel batches concurrent fsyncs automatically

**This is the modern approach for new systems!**

### 4. Client Pipelining vs Server Batching

**Different architectural patterns**:

| Pattern | Who batches? | ACK timing | Durability at ACK |
|---------|--------------|------------|-------------------|
| **Kafka** | Client pipelines | Before fsync | Weak |
| **PostgreSQL** | Server queues | After fsync | Strong |
| **Redpanda** | Kernel (io_uring) | After fsync | Strong |

**Chronik should choose based on durability requirements**, not just throughput.

---

## Recommendations for Chronik

### Current State Analysis

Chronik currently uses:
- GroupCommitWal with thresholds (similar to Kafka's background flush)
- Async response pipeline (decouples ACK from fsync)
- But: ACKs sent before fsync completes (like Kafka acks=1)

### Path Forward: Three Options

#### Option 1: Keep Current Design (Kafka-like)
- **Pros**: Maximum throughput, simple implementation
- **Cons**: Weak durability guarantee (power failure risk)
- **When**: If you explicitly want "eventual durability" like Kafka

#### Option 2: Add io_uring Async Fsync (Redpanda-like)
- **Pros**: Strong durability + high throughput
- **Cons**: Requires Linux 5.1+, complex integration
- **When**: If you want best-in-class performance with durability

#### Option 3: Add Server-Side Group Commit (PostgreSQL-like)
- **Pros**: Simple implementation, strong durability, works on all OSes
- **Cons**: Lower throughput than io_uring, adds latency
- **When**: If you prioritize simplicity and ACID-like guarantees

### Concrete Implementation Guidance

**For Option 3 (Server-Side Group Commit)**:

```rust
// Add to GroupCommitWal
pub struct GroupCommitWal {
    // Existing fields...

    // New: Wait queue for group commit
    commit_queue: Arc<Mutex<CommitQueue>>,
    commit_delay_us: u64,        // Like PostgreSQL commit_delay
    min_concurrent_writes: usize, // Like PostgreSQL commit_siblings
}

struct CommitQueue {
    waiters: Vec<CommitWaiter>,
    last_fsynced_offset: u64,
}

struct CommitWaiter {
    offset: u64,
    waker: Waker,  // For async wakeup
}

impl GroupCommitWal {
    pub async fn append_with_fsync(&self, data: &[u8]) -> Result<u64> {
        // 1. Write to buffer (fast)
        let offset = self.append_internal(data)?;

        // 2. Check if we should group commit
        let queue = self.commit_queue.lock().await;
        let concurrent_writes = queue.waiters.len();

        if concurrent_writes >= self.min_concurrent_writes {
            // Add to wait queue
            queue.waiters.push(CommitWaiter { offset, waker: current_waker() });

            // Sleep briefly to accumulate more writes
            drop(queue);
            tokio::time::sleep(Duration::from_micros(self.commit_delay_us)).await;
        }

        // 3. First to acquire lock does fsync for everyone
        let mut queue = self.commit_queue.lock().await;
        if queue.last_fsynced_offset >= offset {
            // Someone else already fsynced our data
            return Ok(offset);
        }

        // 4. Fsync (covers all waiters)
        self.file.sync_all()?;
        queue.last_fsynced_offset = offset;

        // 5. Wake all waiters
        for waiter in queue.waiters.drain(..) {
            waiter.waker.wake();
        }

        Ok(offset)
    }
}
```

**Key design choice**: With this approach, you can:
- Send ACKs **AFTER** fsync (strong durability like PostgreSQL)
- Still get batching benefits (group commit under load)
- No client-side changes needed (producer just sends requests)

---

## Conclusion

**The fundamental tradeoff**:

1. **Kafka**: Fast ACKs (before fsync) + client pipelining = maximum throughput, weak durability
2. **Redpanda**: Fast ACKs (after async fsync) + io_uring = high throughput, strong durability
3. **PostgreSQL**: Grouped ACKs (after fsync) + server batching = moderate throughput, strong durability

**For Chronik**, the choice depends on whether:
- You want Kafka compatibility (keep current design)
- You want modern performance (adopt io_uring)
- You want ACID-like guarantees (add server-side group commit)

**All three are valid** - the question is which durability/performance trade-off matches your use case.
