# Throughput Investigation (v2.2.7)

**Date:** 2025-11-09
**Issue:** Benchmark shows initial burst (~691 msg/s for 5s), then throughput drops to 0
**Status:** 🔍 INVESTIGATING

---

## Initial Analysis

### Benchmark Pattern (from BENCHMARK_RESULTS_v2.2.7.md)

```
Requested: 128 concurrent producers, 256-byte messages, 15 seconds, acks=1 (default)
Observed:
- Initial burst: ~3456 messages in 5 seconds (~691 msg/s)
- Then stops: 0 msg/s for remaining time
```

### Server Log Analysis (Node 1)

**Timeline:**
- First produce request: `23:14:58.455347Z`
- Last produce request: `23:17:36.290198Z`
- Duration: **158 seconds** (not 5 seconds!)
- Total produce requests logged: **1,679 requests**
- Actual request rate: **10.6 req/s**

**Key Findings:**

1. **Produce requests ARE arriving continuously** (not stopping after 5 seconds)
2. **Request rate is very low**: Only 10.6 req/s with 128 concurrent producers
3. **Raft consensus is active**: Persisting 20-50 entries per batch, ~100ms intervals
4. **Ongoing leader election failures**: "No ISR found for partition chronik-default-0" every 3s
5. **No explicit errors in produce path**: No timeouts, no backpressure warnings

### Possible Root Causes

#### Hypothesis 1: ISR Replication Blocking (acks=-1 mode)
**Evidence:**
- Code at [produce_handler.rs:1448](../crates/chronik-server/src/produce_handler.rs#L1448) waits for ISR quorum with 30s timeout
- If benchmark is using `acks=-1` (all), each produce waits for followers to ACK
- With 128 concurrent producers, this creates massive backpressure

**Counter-evidence:**
- chronik-bench default is `acks=1` (not -1)
- acks=1 returns immediately without ISR wait (line 1411-1419)

#### Hypothesis 2: Raft Consensus Bottleneck
**Evidence:**
- Raft log persisting every ~100ms (lines "Persisting 23 Raft entries to WAL")
- Each batch takes ~20-30ms to persist
- Raft snapshots creating/truncating frequently
- High volume of "unpersisted messages to send" (50-90 messages)

**Analysis:**
- Raft consensus latency: ~20-30ms per batch
- If produces wait for Raft commit, this limits throughput to ~30-50 req/s max
- But we're seeing only 10.6 req/s, so something else is limiting

#### Hypothesis 3: Leader Election Failures Causing Partition Unavailability
**Evidence:**
- Continuous errors: "❌ Failed to elect leader for chronik-default-0: No ISR found for partition"
- Warnings every 3s: "Leader timeout for chronik-default-0 (leader=1), triggering election"
- Partition: `chronik-default-0` (the default topic for metadata?)

**Analysis:**
- If partition leadership is unstable, produces may be blocked/retried
- "No ISR found" suggests ISR tracking is broken
- This could cause produces to fail or retry continuously

#### Hypothesis 4: Client-Side Backpressure/Throttling
**Evidence:**
- chronik-bench uses librdkafka (C++ Kafka client)
- Default librdkafka settings may throttle on errors or slow brokers
- 128 concurrent producers share connection pool

**Analysis:**
- If librdkafka detects slow broker responses, it may back off
- Connection limits or buffer limits could throttle producers
- Need to check librdkafka logs/metrics

### Data Gaps

1. **Missing chronik-bench output**: No benchmark results showing actual msg/s reported by client
2. **Missing WAL replication metrics**: Don't know if followers are ACKing successfully
3. **Missing ISR status**: "No ISR found" suggests ISR tracking broken, but don't see ISR updates
4. **Missing produce latency**: Don't know how long each produce request takes
5. **Missing client logs**: No visibility into librdkafka behavior

---

## Next Steps

### Immediate Investigation

1. **Check if ISR tracking is working**:
   - Search for ISR update logs in all 3 nodes
   - Check if replicas are successfully ACKing WAL replication
   - Verify `isr_ack_tracker` is initialized in cluster mode

2. **Measure produce latency**:
   - Add timing logs to `handle_produce` entry/exit
   - Check if produces are taking 100ms+ (Raft latency)
   - Verify acks=1 is NOT waiting for Raft commit

3. **Check Raft proposal path**:
   - Trace produce → WAL write → Raft propose → commit
   - Verify acks=1 doesn't block on Raft consensus
   - Check if metadata proposals are blocking data proposals

4. **Run benchmark with debug logging**:
   ```bash
   RUST_LOG=chronik_server::produce_handler=debug,chronik_server::raft_cluster=debug \
   ./target/release/chronik-server start --config node1.toml
   ```

### Code Locations to Examine

1. [produce_handler.rs:1411-1419](../crates/chronik-server/src/produce_handler.rs#L1411-L1419) - acks=1 path
2. [produce_handler.rs:1403-1490](../crates/chronik-server/src/produce_handler.rs#L1403-L1490) - All acks modes
3. [raft_cluster.rs](../crates/chronik-server/src/raft_cluster.rs) - Raft message loop and consensus
4. [leader_election.rs](../crates/chronik-server/src/leader_election.rs) - ISR tracking and leader election
5. [wal_replication.rs](../crates/chronik-server/src/wal_replication.rs) - Follower ACK handling

---

## Hypothesis Ranking

1. **Most Likely: Raft consensus blocking produce path** (even for acks=1)
   - Severity: HIGH
   - Evidence: Strong (timing correlates with Raft batching)
   - Fix: Decouple acks=1 from Raft consensus wait

2. **Likely: ISR tracking broken, causes leader election failures**
   - Severity: HIGH
   - Evidence: Strong (continuous "No ISR found" errors)
   - Fix: Fix ISR initialization/tracking in cluster mode

3. **Possible: Client-side throttling due to slow responses**
   - Severity: MEDIUM
   - Evidence: Weak (need client logs to confirm)
   - Fix: Tune librdkafka settings or fix server latency

4. **Unlikely: ISR replication blocking (acks=-1)**
   - Severity: N/A
   - Evidence: None (benchmark uses acks=1 by default)
   - Fix: N/A

---

## Recommended Fix Strategy

### Phase 1: Measure & Confirm (Complete diagnostic picture)
1. Add produce latency logging
2. Add ISR status logging
3. Add Raft propose/commit timing
4. Run benchmark again with debug logs

### Phase 2: Fix ISR Tracking (Critical for cluster stability)
1. Fix "No ISR found" error
2. Ensure ISR updates propagate via Raft
3. Verify leader election stabilizes

### Phase 3: Optimize Produce Path (Performance)
1. Ensure acks=1 doesn't wait for Raft commit
2. Batch Raft proposals more aggressively
3. Reduce Raft consensus latency if possible

### Phase 4: Re-benchmark (Verify improvements)
1. Run chronik-bench with same parameters
2. Compare throughput: target >10K msg/s for 128 producers
3. Verify no errors or leader election failures

---

**Status**: Awaiting Phase 1 diagnostic results

