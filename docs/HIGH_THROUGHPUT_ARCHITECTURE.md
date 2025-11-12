# High-Throughput Architecture Design for Chronik

## Problem Analysis

### Current Bottleneck (v2.2.7)
**Symptom**: Sustained throughput stops at ~130K messages after 5 seconds
- Achieves 26K msg/s peak rate
- Then drops to 0 msg/s and times out

**Root Cause**: TCP Backpressure Deadlock
```
1. Client sends 130K produce requests rapidly (26K msg/s)
2. Server processes them and queues 130K responses (32MB data)
3. TCP send buffer fills (~2-16MB depending on system)
4. write_all() blocks waiting for buffer space
5. Client blocks waiting for delivery confirmations
6. DEADLOCK: Neither side can proceed
```

**Why Current Fixes Don't Work**:
- ✅ Removed clone() - Helped, but doesn't fix TCP blocking
- ✅ Removed logging - Helped, but doesn't fix TCP blocking
- ✅ Batched flush() - Helped, but `flush()` still blocks when buffer is full
- ❌ **The fundamental issue**: Synchronous TCP writes block the response writer task

## Kafka's Architecture (What Makes It Fast)

### 1. Zero-Copy Sendfile
- Uses `sendfile()` syscall to transfer data directly from disk → network
- Bypasses user space entirely (no CPU copies)
- Chronik opportunity: Implement for segment files

### 2. Batching at Protocol Level
- Producers batch multiple messages into one ProduceRequest
- Reduces per-message overhead dramatically
- Network efficiency: 1 TCP roundtrip for 1000s of messages

### 3. Async I/O with Non-Blocking Sockets
- Uses Java NIO (non-blocking I/O)
- `Selector` pattern: poll multiple sockets without blocking
- Can handle 1000s of connections efficiently

### 4. Producer Pacing (Back-Pressure)
- `buffer.memory` config limits in-flight data
- Producer blocks when buffer full (prevents overwhelming broker)
- Flow control: Client adapts to server capacity

### 5. Page Cache Optimization
- Relies heavily on OS page cache
- Sequential writes are cache-friendly
- Reads often served from RAM, not disk

## Proposed Architecture: Ultra-High-Throughput Design

### Phase 1: Non-Blocking Response Writer (CRITICAL)

Replace blocking `write_all()` + `flush()` with truly non-blocking I/O:

```rust
// Current (BLOCKING - BAD):
socket_writer.write_all(&resp_data).await?;  // Blocks when TCP buffer full
socket_writer.flush().await?;                 // Blocks waiting for network

// Proposed (NON-BLOCKING - GOOD):
use tokio::io::AsyncWriteExt;
use tokio::select;

loop {
    select! {
        // Try to write when socket is writable
        Ok(()) = socket_writer.writable() => {
            match socket_writer.try_write(&pending_data) {
                Ok(n) => {
                    // Wrote n bytes, continue
                    pending_data.advance(n);
                }
                Err(ref e) if e.kind() == WouldBlock => {
                    // Socket not ready, try next iteration
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        // Receive new responses while waiting for socket
        Some(response) = response_rx.recv() => {
            pending_responses.insert(response);
        }
    }
}
```

**Benefits**:
- Never blocks on TCP buffer full
- Can continue receiving responses while waiting for socket
- Natural back-pressure: channel fills → request processor slows → client paces itself

### Phase 2: Vectored I/O (writev)

Batch multiple responses into single syscall:

```rust
use tokio::io::AsyncWrite;
use std::io::IoSlice;

// Collect up to 100 consecutive responses
let mut buffers: Vec<IoSlice> = Vec::with_capacity(100);
for seq in next_sequence..next_sequence+100 {
    if let Some((_, data)) = pending_responses.get(&seq) {
        buffers.push(IoSlice::new(data));
    } else {
        break;
    }
}

// Write all buffers in single syscall
match socket_writer.write_vectored(&buffers).await {
    Ok(n) => { /* handle partial writes */ }
    Err(e) => { /* handle error */ }
}
```

**Benefits**:
- Reduces syscall overhead (100 responses = 1 syscall instead of 100)
- Better TCP segment utilization
- 10-100x fewer context switches

### Phase 3: Producer Batching

Implement Kafka-style batch API:

```rust
// Client API:
let mut batch = producer.batch();
for msg in messages {
    batch.send(msg);  // Buffers locally
}
batch.flush().await?;  // Sends all as one ProduceRequest

// Server-side:
// ProduceRequest can contain 1000s of messages
// Server processes all at once, sends single ProduceResponse
```

**Benefits**:
- Amortize network latency over many messages
- Reduce per-message overhead
- Kafka achieves 1M+ msg/s this way

### Phase 4: TCP Tuning

Optimize TCP parameters for high throughput:

```rust
use tokio::net::TcpSocket;

let socket = TcpSocket::new_v4()?;

// Increase send/receive buffers (default: 2MB, set to 16MB)
socket.set_send_buffer_size(16 * 1024 * 1024)?;
socket.set_recv_buffer_size(16 * 1024 * 1024)?;

// Enable TCP_NODELAY (disable Nagle's algorithm for low latency)
socket.set_nodelay(true)?;

// Enable TCP_QUICKACK (reduce delayed ACK latency)
#[cfg(target_os = "linux")]
unsafe {
    use libc::{setsockopt, IPPROTO_TCP, TCP_QUICKACK};
    let fd = socket.as_raw_fd();
    let optval: libc::c_int = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_QUICKACK,
               &optval as *const _ as *const _,
               std::mem::size_of_val(&optval) as u32);
}
```

**System Tuning** (sysctl):
```bash
# Increase TCP buffer limits
net.core.rmem_max = 134217728          # 128MB
net.core.wmem_max = 134217728          # 128MB
net.ipv4.tcp_rmem = 4096 87380 67108864  # min default max
net.ipv4.tcp_wmem = 4096 65536 67108864  # min default max

# Enable TCP window scaling
net.ipv4.tcp_windowադւռscaling = 1

# Reduce TIME_WAIT sockets
net.ipv4.tcp_fin_timeout = 15
net.ipv4.tcp_tw_reuse = 1
```

### Phase 5: Zero-Copy for Large Messages

For messages > 64KB, use sendfile/splice:

```rust
use tokio::fs::File;
use nix::sys::sendfile;

// For segment files:
let file = File::open(segment_path).await?;
let socket_fd = socket.as_raw_fd();
let file_fd = file.as_raw_fd();

// Zero-copy transfer: file → socket (no userspace copy)
sendfile(socket_fd, file_fd, offset, count)?;
```

**Benefits**:
- No CPU copies
- No memory allocation
- Kernel does DMA transfer directly
- Used by Kafka for fetch operations

### Phase 6: Connection Pooling & Load Balancing

Distribute load across multiple connections:

```rust
struct ConnectionPool {
    connections: Vec<Arc<Connection>>,
    next_conn: AtomicUsize,
}

impl ConnectionPool {
    fn get_connection(&self) -> Arc<Connection> {
        let idx = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.connections[idx % self.connections.len()].clone()
    }
}

// Client opens N connections (e.g., 8)
// Requests distributed round-robin
// Parallelizes socket I/O
```

**Benefits**:
- Parallelizes TCP send/receive
- Reduces head-of-line blocking
- Better CPU utilization

## Implementation Plan

### Milestone 1: Non-Blocking I/O (Week 1)
**Goal**: Eliminate TCP backpressure deadlock
- Replace `write_all()` + `flush()` with `try_write()` + `select!`
- Implement proper error handling for partial writes
- Add back-pressure metrics
- **Target**: Sustained 10K msg/s for 30 seconds

### Milestone 2: Vectored I/O (Week 2)
**Goal**: Reduce syscall overhead
- Implement `write_vectored()` for batched writes
- Optimize buffer management
- **Target**: Sustained 50K msg/s

### Milestone 3: TCP Tuning (Week 2)
**Goal**: Optimize network stack
- Implement socket option tuning
- Document system tuning requirements
- **Target**: Sustained 100K msg/s

### Milestone 4: Producer Batching (Week 3)
**Goal**: Reduce per-message overhead
- Implement batch API in protocol
- Update client benchmarks
- **Target**: Sustained 500K msg/s

### Milestone 5: Zero-Copy (Week 4)
**Goal**: Eliminate memory copies
- Implement sendfile for fetch operations
- Optimize segment file layout
- **Target**: Sustained 1M+ msg/s

## Performance Targets

| Metric | Current | Milestone 1 | Milestone 5 | Kafka |
|--------|---------|-------------|-------------|-------|
| Peak Rate | 26K msg/s | 10K msg/s | 1M+ msg/s | 800K msg/s |
| Sustained Rate | 0 (deadlock) | 10K msg/s | 1M+ msg/s | 800K msg/s |
| Latency p99 | 5ms | 10ms | 5ms | 10ms |
| Throughput | 6.4 MB/s | 2.5 MB/s | 250+ MB/s | 200 MB/s |
| CPU Usage | 100% (blocked) | 60% | 80% | 70% |

## Why This Will Beat Kafka

1. **Rust Zero-Cost Abstractions**: No GC pauses (Kafka has 50-200ms GC pauses)
2. **Lock-Free Data Structures**: Crossbeam channels faster than Java's ArrayBlockingQueue
3. **Compiled Native Code**: No JIT warmup, consistent performance from start
4. **Custom Memory Allocator**: Can use jemalloc for better memory efficiency
5. **Integrated Search**: Tantivy indexes built-in (Kafka needs external systems)

## Testing Strategy

### Load Test Scenarios

**Test 1: Sustained Throughput**
```bash
# 30 seconds, 128 concurrent producers, 256-byte messages
# Target: 1M messages (33K msg/s sustained)
chronik-bench --duration 30s --concurrency 128 --message-size 256
```

**Test 2: Large Messages**
```bash
# 10MB messages, test zero-copy
chronik-bench --duration 60s --concurrency 10 --message-size 10485760
```

**Test 3: Mixed Workload**
```bash
# 50% produce, 50% fetch, realistic traffic
chronik-bench --mode mixed --duration 300s
```

**Test 4: Backpressure Resilience**
```bash
# Slow consumer, verify no deadlock
chronik-bench --produce-rate 100000 --consume-rate 1000
```

## Conclusion

The current architecture is limited by **synchronous TCP operations** that block when the network buffer fills. The proposed non-blocking architecture with vectored I/O, batching, and zero-copy will enable:

- ✅ **Sustained high throughput** (no more deadlocks)
- ✅ **Better than Kafka performance** (no GC, native code, lock-free)
- ✅ **Resilience under load** (proper back-pressure)
- ✅ **Lower latency** (no JIT, no GC pauses)

**Next Step**: Implement Milestone 1 (Non-Blocking I/O) - this alone will fix the deadlock and enable sustained throughput.
