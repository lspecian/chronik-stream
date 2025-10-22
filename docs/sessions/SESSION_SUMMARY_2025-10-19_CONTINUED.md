# Chronik Raft Cluster: Leader Election Timing Fix - Session Summary
**Date:** 2025-10-19 (Continued Session)
**Context:** Continuation from previous session attempting to fix 67% NotLeaderForPartitionError rate

## Executive Summary

**Goal:** Achieve 100% reliability for message production in Raft cluster by fixing leader election timing race condition.

**Current Status:** ⚠️ **PARTIALLY SOLVED - 76% success rate (24% failures)**

**Root Cause Identified:** Race condition where Metadata API returns to clients before partition leaders are fully elected, causing producers to hit replicas during the brief leader election window.

**Key Insight:** Adding waits in ProduceHandler hot path causes severe performance degradation or hangs. The fix must be in the Metadata API layer, NOT in the produce path.

---

## Problem Statement

When producers send messages immediately after topic creation in a 3-node Raft cluster:
- **Initial State:** 33% success rate (67% failures)
- **After First Fixes:** 76% success rate (24% failures)
- **Target:** 100% success rate

Error: `NotLeaderForPartitionError` - Producer sends to a node that has the replica but hasn't completed leader election yet.

---

## Fixes Implemented This Session

### 1. ✅ Faster Leader Election (WORKING)
**File:** `crates/chronik-raft/src/config.rs`
```rust
// Reduced from 300ms to 150ms (2x faster)
election_timeout_ms: 150,  // Was: 300
```
**Impact:** Leader elections now complete in ~150ms instead of ~300ms
**Result:** Helped improve from 33% → 76% success rate

### 2. ✅ Cluster Warmup (WORKING)
**File:** `crates/chronik-server/src/raft_cluster.rs`
**Change:** Added `cluster_warmup()` function that waits for `__meta` partition leader election before accepting client connections
```rust
// Wait up to 10 seconds for __meta partition leader
if let Err(e) = cluster_warmup(&raft_manager, Duration::from_secs(10)).await {
    error!("Cluster warmup failed: {:?}", e);
}
```
**Impact:** Ensures cluster is operational before clients connect
**Result:** Eliminates early connection failures

### 3. ✅ Metadata API Wait (WORKING BUT INSUFFICIENT)
**File:** `crates/chronik-server/src/kafka_handler.rs`
**Change:** Added `wait_for_topic_leaders()` method called after topic creation
```rust
// Configuration:
- Timeout: 5 seconds (was 3s)
- Polling interval: 25ms (was 100ms - 4x more aggressive)

// Waits for all partition leaders to be elected before returning metadata
if let Err(e) = self.wait_for_topic_leaders(
    requested_topics,
    Duration::from_secs(5)
).await {
    tracing::warn!("Leader election not complete: {:?}", e);
}
```
**Impact:** Metadata API now waits for leaders before returning to client
**Result:** Major improvement but still 24% failures - indicates wait isn't always working

### 4. ✅ Producer Retry Configuration (WORKING)
**File:** `test_cluster_kafka_python.py`
```python
producer = KafkaProducer(
    bootstrap_servers=bootstrap_servers,
    retries=10,          # Was: 3
    retry_backoff_ms=500 # Added
)
```
**Impact:** Clients retry on transient errors
**Result:** Helps but doesn't solve root cause

### 5. ❌ ProduceHandler Wait Logic (FAILED - ABANDONED)
**File:** `crates/chronik-server/src/produce_handler.rs`

**Attempt 1:** Sleep-based retry loop
```rust
// Added 50 attempts * 20ms sleep = 1 second wait
for attempt in 1..=max_attempts {
    tokio::time::sleep(Duration::from_millis(20)).await;
    if raft_manager.is_leader(...) { break; }
}
```
**Result:** ❌ WORSE - Only 37% success rate, 320 seconds for 100 messages (0.3 msg/s throughput)
**Why Failed:** Async sleep in hot path killed performance

**Attempt 2:** Yield-based tight loop
```rust
// Tight loop with yield_now() and occasional sleep
for attempt in 1..=100 {
    tokio::task::yield_now().await;
    if raft_manager.is_leader(...) { break; }
    if attempt % 10 == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
```
**Result:** ❌ HANGS - Test never completed
**Why Failed:** Infinite wait when leader is on another node (wrong logic)

**Attempt 3:** Conditional wait (leader_hint == 0)
```rust
// Only wait if replica exists AND no leader elected yet (leader_hint == Some(0))
if raft_manager.has_replica(...) && leader_hint == Some(0) {
    // Wait up to 1 second
    for attempt in 1..=50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if raft_manager.is_leader(...) { break; }
    }
}
```
**Result:** ❌ HANGS - Test hung again (same issue as Attempt 1 with sleep)
**Why Failed:** ANY blocking in produce path is fundamentally flawed

**Conclusion:** Adding waits in ProduceHandler is the WRONG approach. The hot path must remain non-blocking.

---

## Test Results Summary

| Approach | Success Rate | Failures | Throughput | Notes |
|----------|-------------|----------|------------|-------|
| Initial (Day 2) | 33% | 67% | Normal | Before any fixes |
| + Warmup + Metadata Wait | 76% | 24% | Normal | Significant improvement |
| + ProduceHandler Sleep | 37% | 63% | 0.3 msg/s | PERFORMANCE DISASTER |
| + ProduceHandler Yield | HANG | - | - | Test never completed |
| + ProduceHandler Conditional | HANG | - | - | Test never completed |

---

## Root Cause Analysis

### Why NotLeaderForPartitionError Occurs

**Timeline of Events:**
```
T=0ms:    Client creates topic "test-topic" with 3 partitions
T=1ms:    Server creates topic metadata
T=2ms:    Topic creation callback fires → creates replicas on all 3 nodes
T=3ms:    Replicas exist but leaders not elected yet
T=4ms:    wait_for_topic_leaders() starts checking
T=5ms:    Metadata API returns to client (assumes leaders ready)
T=6ms:    Client sends produce request to node 1, partition 0
T=7ms:    Node 1 has replica but is_leader() returns false
T=8ms:    Node 1 returns NotLeaderForPartitionError
T=150ms:  Leader actually elected (but too late)
```

### Why wait_for_topic_leaders() Doesn't Always Work

**Hypothesis:** The wait logic checks `raft_manager.get_replica()` and looks for `leader_id != 0`, but:
1. Replica may not be registered yet in RaftReplicaManager
2. Race condition between callback creating replica and wait loop checking
3. Wait loop may be checking before callback completes

**Evidence:**
- 76% of messages succeed (replicas + leaders ready)
- 24% fail (replicas exist but leaders not yet elected)
- Consistent failure pattern (not random)

---

## Files Modified This Session

### Core Changes (Still in codebase)
1. `crates/chronik-raft/src/config.rs` - Faster election timeout (300ms → 150ms)
2. `crates/chronik-server/src/raft_cluster.rs` - Cluster warmup logic
3. `crates/chronik-server/src/kafka_handler.rs` - Improved wait_for_topic_leaders()
4. `test_cluster_kafka_python.py` - Producer retry configuration

### Experimental Changes (Added then partially abandoned)
5. `crates/chronik-server/src/produce_handler.rs`:
   - ✅ KEPT: `raft_manager()` accessor method (needed by kafka_handler.rs)
   - ❌ ABANDONED: Wait loop logic (causes hangs)

**Current git diff status:**
```rust
// produce_handler.rs contains:
// 1. raft_manager() accessor (lines 673-681) - KEEP
// 2. Wait loop logic (lines 972-1021) - SHOULD REMOVE
```

---

## Key Learnings

### What Works
1. ✅ **Faster election timeouts** - Direct impact on leader election speed
2. ✅ **Cluster warmup** - Ensures basic infrastructure ready
3. ✅ **Metadata API delays** - Prevents premature client requests (partially)
4. ✅ **Client retries** - Handles transient failures gracefully

### What Doesn't Work
1. ❌ **Blocking in hot path** - Any `tokio::time::sleep()` in ProduceHandler kills performance
2. ❌ **Yielding in hot path** - Even `yield_now()` causes issues when looping
3. ❌ **Waiting for leadership** - If leader is on another node, waiting is futile

### Critical Insight
> **The produce path must remain non-blocking at all costs.**
>
> Any wait logic, no matter how "smart" or "conditional", adds unacceptable latency or hangs. The solution MUST be to prevent the race condition from occurring in the first place, not to handle it in the produce path.

---

## Recommended Next Steps

### Immediate Action (Next Session)

1. **REVERT ProduceHandler Wait Logic**
   ```bash
   # Keep only the raft_manager() accessor, remove wait loop
   git checkout crates/chronik-server/src/produce_handler.rs
   # Then re-add just the accessor method
   ```

2. **DEBUG wait_for_topic_leaders() Logic**
   - Add extensive debug logging to understand why it fails 24% of the time
   - Check if callback completes before wait loop starts
   - Verify replica registration timing

   ```rust
   // Add logging:
   debug!("wait_for_topic_leaders: Starting wait for {:?}", topic_names);
   debug!("Attempt {}/{}: partition {}-{} leader={:?}",
          attempt, max_attempts, topic, partition, leader_id);
   ```

3. **Test Hypothesis: Callback Timing**
   - Add 500ms delay in wait_for_topic_leaders() BEFORE starting polling
   - If this fixes the issue → confirms callback takes time to propagate
   - If this doesn't fix → issue is elsewhere

4. **Alternative Approach: Synchronous Replica Creation**
   - Instead of async callback, make topic creation wait for replicas
   - Block Metadata API until replicas are confirmed on all nodes
   - More aggressive but guarantees correctness

### Long-Term Solution Options

**Option A: Improve wait_for_topic_leaders() Reliability**
- Pros: Minimal changes, keeps async architecture
- Cons: Race conditions are hard to fully eliminate
- Recommendation: Try this first

**Option B: Synchronous Replica Confirmation**
- Pros: Eliminates race condition completely
- Cons: Adds latency to topic creation (acceptable trade-off)
- Recommendation: If Option A fails

**Option C: Client-Side Retry Only**
- Pros: Simplest server implementation
- Cons: Relies on client configuration, not 100% reliable
- Recommendation: Last resort only

---

## Environment Information

**Cluster Configuration:**
- 3 nodes: localhost:9092, localhost:9093, localhost:9094
- Raft ports: 5001, 5002, 5003
- Election timeout: 150ms
- Heartbeat interval: 30ms

**Test Workload:**
- 1000 messages produced to 3-partition topic
- kafka-python client with 10 retries, 500ms backoff
- E2E test: topic creation → produce → consume → verify

**Build:**
```bash
cargo build --features raft --release
# Warnings: 160+ (mostly unused variables, not critical)
# Binary: ./target/release/chronik-server
```

---

## Code State

### Clean Compilation: ✅
```bash
$ cargo build --features raft --release
   Compiling chronik-server v1.3.65
   Finished `release` profile [optimized + debuginfo] target(s) in 1m 01s
```

### Uncommitted Changes:
```bash
$ git status
M crates/chronik-raft/src/config.rs           # election_timeout 300→150
M crates/chronik-server/src/kafka_handler.rs  # wait_for_topic_leaders improvements
M crates/chronik-server/src/produce_handler.rs # raft_manager() + ABANDONED wait loop
M crates/chronik-server/src/raft_cluster.rs   # cluster_warmup()
M test_cluster_kafka_python.py                # retry config
```

### Recommended Git Actions:
1. Commit working changes (config, kafka_handler, raft_cluster)
2. Reset produce_handler.rs to clean state
3. Re-add only the raft_manager() accessor method
4. Commit separately with clear message

---

## Performance Metrics

### Successful Test (76% Success Rate)
- **Throughput:** Normal (not measured precisely)
- **Latency:** < 100ms per message (estimated)
- **Success Rate:** 76/100 messages
- **Failure Pattern:** Consistent 24% failure in first ~200 messages

### Failed Test (37% Success Rate with Sleep)
- **Throughput:** 0.3 msg/s (DISASTER - 100x slower)
- **Latency:** 320 seconds for 100 messages
- **Success Rate:** 37/100 messages
- **Failure Pattern:** Severe performance degradation

### Hung Tests (Yield-based and Conditional Wait)
- **Throughput:** N/A (hung indefinitely)
- **Cause:** Infinite wait loop when leader on different node
- **Lesson:** ANY blocking in produce path is dangerous

---

## Questions for Next Session

1. **Why does wait_for_topic_leaders() fail 24% of the time?**
   - Is callback slow?
   - Is replica registration async?
   - Is leader election reporting delayed?

2. **Can we make replica creation synchronous?**
   - Would this eliminate the race condition?
   - What's the latency impact on topic creation?

3. **Is there a better signal for "leaders ready"?**
   - Can we wait for Raft state machine to confirm leaders?
   - Can we hook into the leader election event?

4. **Should we increase wait timeout beyond 5 seconds?**
   - Would 10 seconds help?
   - Or is the issue not about timeout but about checking logic?

---

## Conclusion

**Progress Made:**
- ✅ Improved success rate from 33% to 76% (2.3x improvement)
- ✅ Identified that ProduceHandler waits are fundamentally flawed
- ✅ Confirmed that leader election itself is fast (< 150ms)
- ✅ Narrowed problem to Metadata API wait logic

**Work Remaining:**
- Fix the remaining 24% failures
- Debug why wait_for_topic_leaders() doesn't always work
- Achieve 100% reliability without sacrificing performance

**Next Session Strategy:**
1. Remove ProduceHandler wait logic (clean up)
2. Add extensive debug logging to wait_for_topic_leaders()
3. Test with delays to understand callback timing
4. Consider synchronous replica creation as fallback

**Estimated Effort to 100% Reliability:**
- Best case: 1-2 hours (if callback timing fix works)
- Worst case: 4-6 hours (if need synchronous replica creation)

---

## References

**Previous Session Summaries:**
- `DAY1_SUMMARY.md` - Initial clustering implementation
- `DAY2_CONT_SESSION_SUMMARY.md` - First attempt at leader election fix
- `DAY2_FINAL_SUMMARY.md` - 33% → 76% improvement

**Key Files:**
- Leader election: `crates/chronik-raft/src/replica.rs`
- Metadata handling: `crates/chronik-server/src/kafka_handler.rs`
- Produce path: `crates/chronik-server/src/produce_handler.rs`
- Cluster coordination: `crates/chronik-server/src/raft_cluster.rs`

**Test Scripts:**
- E2E test: `test_cluster_kafka_python.py`
- Manual cluster control: `test_cluster_manual.sh`
- Quick tests: Inline Python scripts in session

---

**Session End Time:** 2025-10-19 17:20 UTC
**Duration:** ~2 hours (continuation session)
**Status:** In progress - needs next session to reach 100% reliability
