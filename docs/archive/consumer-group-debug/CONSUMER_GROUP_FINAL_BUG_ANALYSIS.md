# Consumer Group Bug Analysis - COMPLETE ROOT CAUSE

**Date**: 2025-11-03
**Status**: TWO BUGS DISCOVERED
**Benchmark**: 128 concurrency, 256 byte messages, 30 seconds, 3 staggered consumers

---

## Executive Summary

Testing with staggered consumer starts revealed **TWO SEPARATE CRITICAL BUGS** in the consumer group implementation:

1. **BUG #1: Join Phase Early Return** ✅ **FIXED**
2. **BUG #2: SyncGroup Race Condition** ❌ **NOT FIXED YET**

### Results

**With Bug #1 Fix Only**:
- Production: 51,277 msg/s @ p99 = 5.9ms ✅ EXCELLENT
- Consumption:
  - Consumer 1: 22,126 messages ✅
  - Consumer 2: 0 messages ❌
  - Consumer 3: 0 messages ❌

---

## BUG #1: Join Phase Early Return (FIXED)

### Problem

When a new member joined a `Stable` group, the code called `trigger_rebalance()` but then **immediately returned a JoinGroup response** without waiting for other members.

**Original Broken Code** ([consumer_group.rs:766-794](../crates/chronik-server/src/consumer_group.rs#L766-L794)):
```rust
} else if group.state == GroupState::Stable {
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

### Root Cause

The 100ms timer-based approach worked for **simultaneous joins** but failed for **staggered joins**:
- Consumer 1 joins → 100ms timer → Completes with 1 member → generation=0
- Consumer 2 joins 1s later → Triggers rebalance → **Immediately returns response** → generation=1
- Consumer 3 joins 2s later → Triggers rebalance → **Immediately returns response** → generation=2

### Solution

Remove the early return - let the code fall through to the `PreparingRebalance` handler that implements correct timer-based waiting.

**Fixed Code** ([consumer_group.rs:629-639](../crates/chronik-server/src/consumer_group.rs#L629-L639)):
```rust
if group.state == GroupState::Stable && !is_rejoin {
    // New member joining stable group - trigger rebalance
    info!(
        group_id = %group_id,
        member_id = %member_id,
        "New member joining stable group - triggering rebalance"
    );
    group.trigger_rebalance();
    // NOTE: trigger_rebalance() sets state to PreparingRebalance
    // Fall through to PreparingRebalance handler below (DO NOT return here!)
}

// Handle PreparingRebalance and Empty states with timer-based waiting
if group.state == GroupState::PreparingRebalance || group.state == GroupState::Empty {
    // Create oneshot channel, spawn 100ms timer, wait for response...
}
```

### Verification

Server logs confirm the fix works:
```
[22:11:17] Consumer 1 joins
[22:11:17] Timer elapsed - completing join phase members=1

[22:11:18] Consumer 2 joins (1 second later)
[22:11:18] New member joining stable group - triggering rebalance
[22:11:18] Timer elapsed - completing join phase members=2  ← ✅ Waits!

[22:11:19] Consumer 3 joins (2 seconds later)
[22:11:19] New member joining stable group - triggering rebalance
[22:11:19] Timer elapsed - completing join phase members=3  ← ✅ Waits!
```

**BUG #1 STATUS**: ✅ **FIXED AND VERIFIED**

---

## BUG #2: SyncGroup Race Condition (NOT FIXED)

### Problem

After JoinGroup completes, **followers call SyncGroup before the leader**, causing them to receive empty assignments.

**Sequence** (from server logs):

**Generation 0** (1 member):
```
[22:11:17.643] Leader computing partition assignments  ← ✅ Leader calls SyncGroup first
[22:11:17.643] Assigned partitions to member partitions=[0]
[22:11:17.648] Member synced group state=Stable
```

**Generation 1** (2 members):
```
[22:11:18.639] Completing join phase member_count=2
[22:11:18.639] Returning assignment to member assignment={}  ← ❌ Follower gets EMPTY!
[22:11:18.639] Member synced group state=Stable
```

**NO "Leader computing" log for generation 1!** The leader never calls SyncGroup in generation 1.

**Generation 2** (3 members):
```
[22:11:19.640] Completing join phase member_count=3
[22:11:19.640] Returning assignment to member assignment={}  ← ❌ Follower gets EMPTY!
[22:11:19.640] Member synced group state=Stable

[22:11:20.652] All members synced - completing rebalance  ← 1 second later!
[22:11:20.658] Member synced group leader=true state=Stable
```

### Root Cause

The bug is in lines 904-912 of `sync_group()`:

```rust
} else {
    // Non-leader member syncing - check if this was the last pending member
    if group.pending_members.is_empty() &&
       (group.state == GroupState::CompletingRebalance || group.state == GroupState::PreparingRebalance) {
        info!(
            group_id = %group.group_id,
            "Last member synced (non-leader) - transitioning to Stable"
        );
        group.state = GroupState::Stable;  // ← ❌ BUG: Transitions to Stable before leader computes assignments!
```

**What happens**:
1. Join phase completes with 3 members in `CompletingRebalance` state
2. All members start calling SyncGroup
3. Follower 1 calls SyncGroup → Removes itself from `pending_members` → Gets empty assignment
4. Follower 2 calls SyncGroup → `pending_members.is_empty()` is TRUE → **Transitions to Stable** → Gets empty assignment
5. Leader calls SyncGroup (much later) → State is now `Stable` → **Condition at line 849-850 is FALSE** → `compute_assignments()` is NEVER called!

```rust
// This check FAILS when state is Stable:
if group.state == GroupState::PreparingRebalance ||
   group.state == GroupState::CompletingRebalance ||
   assignments.as_ref().map(|a| a.is_empty()).unwrap_or(true) {
    // compute_assignments() is never reached!
}
```

### Why This Happens

**rdkafka client behavior**: After receiving JoinGroup response, followers immediately call SyncGroup without waiting. They don't know the leader needs to call SyncGroup first to compute assignments.

**Server should handle this**: The server should NOT return SyncGroup responses to followers until the leader has provided assignments. Followers should BLOCK (like we did with JoinGroup) until assignments are ready.

### Required Fix

The server needs to implement **SyncGroup response blocking** similar to the JoinGroup fix:

1. When a non-leader calls SyncGroup, create a oneshot channel and BLOCK their response
2. When the leader calls SyncGroup with assignments, compute assignments and send responses to ALL waiting members simultaneously
3. Only transition to `Stable` after all members have received their assignments

**Similar to JoinGroup pattern**:
```rust
// Pseudo-code for the fix
pub sync_group(...) {
    if is_leader {
        // Compute assignments
        let assignments = self.compute_assignments(group).await?;

        // Send responses to ALL waiting members (leader + followers)
        for (member_id, assignment) in assignments {
            if let Some(tx) = pending_sync_futures.remove(&member_id) {
                tx.send(SyncGroupResponse { assignment, ... });
            }
        }

        // Transition to Stable
        group.state = GroupState::Stable;
    } else {
        // Create oneshot channel and WAIT for leader to provide assignments
        let (tx, rx) = oneshot::channel();
        pending_sync_futures.insert(member_id, tx);

        // Drop lock and wait
        drop(groups);
        rx.await?  // ← Blocks until leader sends assignments
    }
}
```

---

## Impact Assessment

### What Works ✅
- **JoinGroup phase**: All consumers successfully join and receive JoinGroup responses
- **Timer-based waiting**: Works for both simultaneous and staggered joins
- **Production performance**: 51K msg/s @ p99 = 5.9ms

### What's Broken ❌
- **SyncGroup phase**: Followers call SyncGroup before leader, get empty assignments
- **Partition distribution**: Only first consumer gets partitions
- **Real-world deployments**: Any scenario with staggered consumer starts fails

### Severity: **CRITICAL**

Consumer groups are **completely broken** for real-world deployments where:
- Kubernetes pods start with delays
- Consumers are added dynamically
- Network delays cause staggered arrivals

---

## Action Plan

### Immediate Next Steps

1. **Implement SyncGroup blocking** (similar to JoinGroup fix):
   - Add `pending_sync_futures: Arc<Mutex<HashMap<String, oneshot::Sender<SyncGroupResponse>>>` to `ConsumerGroup`
   - Modify `sync_group()` to block non-leader responses until leader provides assignments
   - Remove early `Stable` transition in follower path

2. **Test with benchmark**:
   - Re-run exact same test (3 consumers, 1-second staggered starts)
   - Verify all 3 consumers receive partition assignments
   - Verify total consumed messages = produced messages

3. **Additional testing**:
   - Test with 5-10 consumers
   - Test with large gaps (10+ seconds between joins)
   - Test adding consumers to running group after hours

---

## Lessons Learned

1. **Protocol compliance is hard**: The Kafka consumer group protocol has subtle ordering requirements that aren't obvious from the spec
2. **Race conditions are real**: Even with the timer-based approach, order of SyncGroup calls matters
3. **Test with real clients**: Chronik-bench (rdkafka) behaves differently than simultaneous test scripts
4. **Comprehensive testing needed**: Testing with simultaneous starts is NOT enough - must test staggered starts

---

## References

- Original benchmark: [CONSUMER_GROUP_BENCHMARK_RESULTS.md](./CONSUMER_GROUP_BENCHMARK_RESULTS.md)
- First fix documentation: [CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md](./CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md)
- Kafka Protocol Spec: https://kafka.apache.org/protocol#The_Messages_SyncGroup
- Server code: [crates/chronik-server/src/consumer_group.rs](../crates/chronik-server/src/consumer_group.rs)
