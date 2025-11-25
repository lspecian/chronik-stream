# Async Request Pipelining Implementation

## Status: IN PROGRESS

### Root Cause Analysis (COMPLETED)

**Problem:** acks=1 shows 168x performance degradation (2,197 msg/s vs 370,451 msg/s for acks=0)

**Root Cause:** SYNCHRONOUS HEAD-OF-LINE BLOCKING in leader forwarding

Current implementation (`produce_handler.rs:1585-1789`):
```rust
// BLOCKING: Each request waits for response before next can start
stream.write_all(request).await;  // Send
let response = stream.read().await;  // WAIT 50ms (WAL fsync)
return response;                    // Finally return
```

**Math:** `1000ms / 50ms per request ≈ 20 req/s per connection × 128 connections ≈ 2,560 msg/s` ✅ Matches observed

**Solution:** Implement Kafka-style async pipelining (100+ requests in-flight simultaneously)

---

## Implementation Progress

### ✅ COMPLETED

1. **Created [pipelined_connection.rs](crates/chronik-server/src/pipelined_connection.rs)** (549 lines)
   - Full async pipelining implementation with correlation ID matching
   - Connection pooling with shared connections per leader

2. **Integrated pipelined connection module**
   - Added `mod pipelined_connection;` to [main.rs:20](crates/chronik-server/src/main.rs#L20)
   - Added import to [produce_handler.rs:51](crates/chronik-server/src/produce_handler.rs#L51)
   - Replaced field in [ProduceHandler struct:480](crates/chronik-server/src/produce_handler.rs#L480)
   - Updated initialization in [`new()`:969](crates/chronik-server/src/produce_handler.rs#L969)

**Architecture Design**
   ```
   Client                Send Task              Receive Task
     │                      │                       │
     ├─ send_request() ────→│                       │
     ├─ send_request() ────→│                       │
     ├─ send_request() ────→│                       │
     │                      ├─ write_all() ────────→│
     │                      ├─ write_all() ────────→│
     │                      ├─ write_all() ────────→│
     │                      │                   ┌───┴─ read_response()
     │← response ───────────┴───────────────────┘ (match correlation_id)
   ```

### 🔨 REMAINING WORK

1. **Add pipelined_connection import to produce_handler.rs**
   - Add after line 47: `use crate::pipelined_connection::{PipelinedConnection, PipelinedConnectionPool};`

2. **Replace LeaderConnectionPool in ProduceHandler struct**
   - Find `LeaderConnectionPool` declaration (around line 180-250)
   - Replace with: `pipelined_pool: Arc<PipelinedConnectionPool>`
   - Update `ProduceHandler::new()` to create pipelined pool:
     ```rust
     pipelined_pool: Arc::new(PipelinedConnectionPool::new(1000)),
     ```

3. **Rewrite forward_produce_to_leader() method** (lines 1585-1789)

   **OLD approach (synchronous):**
   ```rust
   async fn forward_produce_to_leader(...) -> Result<ProduceResponsePartition> {
       let stream = self.connection_pool.get_connection(&leader_addr).await?;

       // Build request frame
       let request_frame = build_request(...);

       // BLOCKING: Send and wait
       stream.write_all(&request_frame).await?;
       let response = stream.read().await?;  // BLOCKS HERE!

       parse_response(response)
   }
   ```

   **NEW approach (pipelined):**
   ```rust
   async fn forward_produce_to_leader(...) -> Result<ProduceResponsePartition> {
       let leader_addr = get_leader_addr(...)?;

       // Get pipelined connection
       let conn = self.pipelined_pool.get_connection(&leader_addr).await?;

       // Build request frame (SAME as before)
       let request_frame = build_produce_request_frame(...);

       // Send request (NON-BLOCKING - returns immediately!)
       let response_frame = conn.send_request(
           request_frame,
           5000,  // 5 second timeout
       ).await?;

       // Parse response (SAME as before)
       parse_produce_response(response_frame)
   }
   ```

4. **Helper function: build_produce_request_frame()**
   - Extract lines 1627-1690 into standalone function
   - Input: topic, partition, records, acks, timeout
   - Output: Bytes (complete request frame)

5. **Helper function: parse_produce_response()**
   - Extract lines 1718-1781 into standalone function
   - Input: Bytes (complete response frame)
   - Output: Result<ProduceResponsePartition>

---

## Testing Plan

1. **Build:**
   ```bash
   cargo build --release --bin chronik-server
   ```

2. **Start 3-node cluster:**
   ```bash
   ./tests/cluster/start.sh
   ```

3. **Test acks=0 (baseline):**
   ```bash
   ./target/release/chronik-bench -c 128 -s 256 -d 30s --acks 0 \
     --topic acks0-baseline --create-topic \
     --bootstrap-servers localhost:9092,localhost:9093,localhost:9094
   ```
   Expected: ~370,000 msg/s @ 0.94ms p99

4. **Test acks=1 (should match acks=0 now!):**
   ```bash
   ./target/release/chronik-bench -c 128 -s 256 -d 30s --acks 1 \
     --topic acks1-pipelined --create-topic \
     --bootstrap-servers localhost:9092,localhost:9093,localhost:9094
   ```
   Expected: ~300,000-370,000 msg/s (100x+ improvement!)

5. **Test acks=all:**
   ```bash
   ./target/release/chronik-bench -c 128 -s 256 -d 30s --acks all \
     --topic acksall-pipelined --create-topic \
     --bootstrap-servers localhost:9092,localhost:9093,localhost:9094
   ```
   Expected: ~200,000-300,000 msg/s (should complete, not hang)

---

## Key Implementation Details

### Correlation ID Management
- Atomic increment starting at 1
- Thread-safe via `AtomicI32`
- Unique per connection (not global)
- Matched in receive task via HashMap<i32, oneshot::Sender>

### Error Handling
- Send failure: Immediate error to caller, connection closed
- Receive failure: All pending requests notified, connection closed
- Timeout: Request removed from pending, timeout error returned
- Caller dropped: Logged as warning, response discarded

### Connection Lifecycle
- Created on first request to leader
- Cached in pool indefinitely
- Shared across all callers (Arc)
- Background tasks survive until TCP error

### Performance Characteristics
- Request queue size: 1000 (configurable)
- Default timeout: 5000ms
- Expected in-flight: 100-500 requests
- Expected throughput: 300,000+ msg/s with acks=1

---

## Files Modified

1. **NEW:** `crates/chronik-server/src/pipelined_connection.rs` (549 lines)
2. **TODO:** `crates/chronik-server/src/produce_handler.rs`
   - Add import (1 line)
   - Replace connection pool (3 lines)
   - Rewrite forward_produce_to_leader() (simplify from 200 lines to ~30 lines)
   - Add helper functions (2 functions, ~100 lines total)

---

## Estimated Completion Time

- Remaining work: ~2-3 hours
- Lines to modify: ~200 lines
- Testing: 30 minutes
- **Total: 3-4 hours**

---

## Rollback Plan

If pipelined implementation has issues:

1. Git commit current work
2. Add feature flag to toggle between sync/async:
   ```rust
   if env::var("CHRONIK_USE_ASYNC_FORWARDING").is_ok() {
       // Use pipelined connection
   } else {
       // Use old synchronous forwarding
   }
   ```
3. Test both modes
4. Fix issues in async path
5. Remove sync path once async is stable

---

## Next Steps

1. Complete integration (steps 1-5 above)
2. Build and test
3. Verify 100x+ performance improvement
4. Document in CLAUDE.md
5. Update version to v2.2.9

---

## Contact

Implementation by: Claude (Anthropic)
Date: 2025-11-23
Issue: acks=1 performance degradation (168x slower than acks=0)
Solution: Async request pipelining like modern Kafka
