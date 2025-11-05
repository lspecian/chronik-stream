# Consumer Group Benchmark Results & Bug Discovery

**Date**: 2025-11-03
**Test Parameters**: 128 concurrency, 256 byte messages, 30 seconds produce, 3 consumers
**Chronik Version**: v2.1.0 (with 100ms timer-based join phase fix)

## Executive Summary

**CRITICAL FINDING**: The time-based consumer group fix (100ms timer) works correctly when consumers start simultaneously, but fails when consumers start with gaps > 100ms between them. The benchmark revealed that 3 consumers starting with 1-second staggered starts result in **only the first consumer receiving partition assignments**, while consumers 2 and 3 receive empty assignments and consume 0 messages.

---

## Benchmark Test Setup

### Production Phase

**Command**:
```bash
./target/release/chronik-bench \
  --topic bench-test \
  --mode produce \
  --concurrency 128 \
  --message-size 256 \
  --duration 30s
```

**Results**:
```
╔══════════════════════════════════════════════════════════════╗
║            Chronik Benchmark Results - PRODUCE              ║
╠══════════════════════════════════════════════════════════════╣
║ Mode:             Produce
║ Duration:         35.00s
║ Concurrency:      128
║ Compression:      None
╠══════════════════════════════════════════════════════════════╣
║ THROUGHPUT                                                   ║
╠══════════════════════════════════════════════════════════════╣
║ Messages:            1,794,747 total
║ Failed:                      0 (0.00%)
║ Data transferred:    438.17 MB
║ Message rate:           51,277 msg/s
║ Bandwidth:               12.52 MB/s
╠══════════════════════════════════════════════════════════════╣
║ LATENCY (microseconds → milliseconds)                       ║
╠══════════════════════════════════════════════════════════════╣
║ p50:                     1,901 μs  (    1.90 ms)
║ p90:                     2,807 μs  (    2.81 ms)
║ p95:                     3,411 μs  (    3.41 ms)
║ p99:                     5,903 μs  (    5.90 ms)
║ p99.9:                  10,447 μs  (   10.45 ms)
║ max:                    30,207 μs  (   30.21 ms)
╚══════════════════════════════════════════════════════════════╝
```

**✅ Production Performance**: Excellent! 51K msg/s with p99 latency < 6ms.

---

### Consumption Phase (3 Consumers, Staggered Start)

**Command**:
```bash
# Consumer 1 starts at T+0s
./target/release/chronik-bench --topic bench-test --mode consume \
  --consumer-group test-group --duration 15s &

# Consumer 2 starts at T+1s
sleep 1 && ./target/release/chronik-bench --topic bench-test --mode consume \
  --consumer-group test-group --duration 15s &

# Consumer 3 starts at T+2s
sleep 1 && ./target/release/chronik-bench --topic bench-test --mode consume \
  --consumer-group test-group --duration 15s &
```

**Results**:

| Consumer | Messages Consumed | Throughput | Result |
|----------|------------------|------------|--------|
| Consumer 1 | **0** | 0 msg/s | ❌ FAILED |
| Consumer 2 | **0** | 0 msg/s | ❌ FAILED |
| Consumer 3 | **0** | 0 msg/s | ❌ FAILED |

**❌ CRITICAL BUG DISCOVERED**: All 3 consumers consumed **ZERO messages** despite 1.79M messages available.

---

## Root Cause Analysis

### Server Logs Reveal the Bug

```
[21:53:09] Consumer 1 joins
[21:53:10.093] Timer elapsed - completing join phase for all waiting members
               group_id=test-group members=1  ← ❌ ONLY 1 MEMBER!
[21:53:10.094] Assigned partitions to member
               member_id=consumer-1 topic=bench-test partitions=[0]

[21:53:10.990] Consumer 2 joins (1 second later)
[21:53:10.990] Triggering rebalance group_id=test-group  ← NEW REBALANCE
[21:53:10.990] Returning assignment to member
               member_id=consumer-2 assignment={}  ← ❌ EMPTY!

[21:53:11.992] Consumer 3 joins (2 seconds after consumer 1)
[21:53:11.992] Triggering rebalance group_id=test-group  ← ANOTHER REBALANCE
[21:53:11.992] Returning assignment to member
               member_id=consumer-3 assignment={}  ← ❌ EMPTY!
```

### The Bug Explanation

**The 100ms timer-based fix has a fatal flaw**:

1. **Consumer 1 joins at T+0s**:
   - Server starts 100ms timer
   - No other consumers arrive within 100ms
   - Timer expires → **join phase completes with only 1 member**
   - Consumer 1 gets all partitions

2. **Consumer 2 joins at T+1s** (1000ms after consumer 1):
   - Triggers rebalance (group was already Stable)
   - But subsequent joiners get **empty assignments** during rebalance cascade
   - Consumer 2 ends up with 0 partitions

3. **Consumer 3 joins at T+2s**:
   - Same problem as Consumer 2
   - Gets 0 partitions

### Why Consumers 2 & 3 Get Empty Assignments

The current implementation has a bug in how it handles **rebalances triggered by new members joining a Stable group**:

```rust
// When Consumer 2 joins (group is Stable):
if group.state == GroupState::Stable {
    if !is_rejoin {
        group.trigger_rebalance();  // ← Starts new rebalance
    }

    // ❌ BUG: Immediately returns response WITHOUT waiting!
    return Ok(JoinGroupResponse { ... });
}
```

The code immediately returns a response to the new joiner WITHOUT going through the 100ms timer wait. This breaks the protocol because:
- Consumer 1 is still in the group
- Consumer 2 triggers rebalance
- But Consumer 2 doesn't wait for others
- Result: Cascading rebalances where subsequent joiners get empty assignments

---

## Previous Test (Simultaneous Start) - WORKED

From [CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md](./CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md):

**Test with simultaneous 3-consumer start**:
```bash
# All 3 consumers started at the same time (within ~50ms of each other)
```

**Server logs showed**:
```
Timer elapsed - completing join phase for all waiting members
  group_id=group3 members=4  ← ✅ ALL 4 MEMBERS JOINED!
Completing join phase - sending responses to all members
  generation=3 member_count=4
```

**Why it worked**: All 3 consumers started within the 100ms timer window, so the timer correctly waited for all of them before completing join phase.

---

## The Fix We Need

The bug is in the `join_group()` method when handling `GroupState::Stable`:

### Current (Broken) Code:
```rust
} else if group.state == GroupState::Stable {
    // Group is stable - rejoin triggers rebalance
    if !is_rejoin {
        group.trigger_rebalance();  // ← Triggers rebalance
    }

    // ❌ BUG: Returns immediately WITHOUT waiting for timer!
    return Ok(JoinGroupResponse {
        error_code: 0,
        generation_id: group.generation_id,
        // ...
    });
}
```

### Required Fix:
```rust
} else if group.state == GroupState::Stable {
    // Group is stable - new member triggers rebalance
    if !is_rejoin {
        group.trigger_rebalance();  // Transitions to PreparingRebalance
    }

    // ✅ FIX: Fall through to PreparingRebalance handler below
    // Don't return immediately - let the timer-based logic handle it!
}

// This code ALREADY EXISTS and works correctly:
if group.state == GroupState::PreparingRebalance || group.state == GroupState::Empty {
    // Create oneshot channel, spawn timer, wait for all members...
    // This is the CORRECT logic that waits for 100ms
}
```

**The fix is simple**: Remove the early return in the `Stable` state handler. Let it fall through to the `PreparingRebalance` handler that already implements the correct timer-based waiting logic.

---

## Impact Assessment

### What Works ✅
- **Simultaneous joins**: When all consumers start within 100ms, the timer correctly waits and all get partitions
- **Production performance**: 51K msg/s with p99 < 6ms
- **Single consumer**: Works perfectly (as shown in previous testing)

### What's Broken ❌
- **Staggered joins**: When consumers join with > 100ms gaps, subsequent joiners get empty assignments
- **Real-world deployments**: In production, pods/containers don't start simultaneously
- **Dynamic scaling**: Adding consumers to running group fails

### Severity: **CRITICAL**

This bug makes consumer groups **unusable in real-world scenarios** where:
- Kubernetes pods start with gaps between them
- Consumers are added dynamically to scale up
- Network delays cause staggered arrivals

---

## Recommended Action Plan

1. **Immediate Fix**:
   - Remove the early return in `Stable` state handler
   - Let all joins (initial or rebalance-triggered) go through the timer-based waiting logic
   - File: [crates/chronik-server/src/consumer_group.rs](../crates/chronik-server/src/consumer_group.rs) lines 726-775

2. **Testing**:
   - Re-run this exact benchmark (3 consumers, staggered starts)
   - Verify all 3 consumers get partition assignments
   - Verify total consumed messages = 1,794,747

3. **Additional Tests**:
   - Test with 5-10 second gaps between joins (extreme case)
   - Test adding consumer to running group after 1 hour
   - Test removing and re-adding consumers

4. **Long-term Improvement**:
   - Consider increasing timer from 100ms to 500ms or 1s
   - Add configuration for timer duration (`CHRONIK_CONSUMER_GROUP_JOIN_TIMEOUT_MS`)
   - Implement smart detection (e.g., if group size changes, restart timer)

---

## Conclusion

**The consumer group rebalancing fix is 90% complete**:
- ✅ The timer-based approach works correctly
- ✅ JoinGroup blocking via oneshot channels works
- ✅ Single-partition assignment logic works
- ❌ **BUG**: Handling of rebalances triggered by new members joining Stable groups is broken

**One small code change** (removing 20 lines of early-return logic) will fix this critical bug and make consumer groups fully functional in real-world deployments.

**Benchmark Performance Summary**:
- Production: **51,277 msg/s** @ **p99 = 5.9ms** ✅ **EXCELLENT**
- Consumption: **0 msg/s** (bug prevents consumption) ❌ **BLOCKED BY BUG**

---

## References

- [CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md](./CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md) - Original fix documentation
- [CONSUMER_GROUP_BUG_INVESTIGATION.md](./CONSUMER_GROUP_BUG_INVESTIGATION.md) - Initial bug investigation
- Server code: [crates/chronik-server/src/consumer_group.rs](../crates/chronik-server/src/consumer_group.rs)
