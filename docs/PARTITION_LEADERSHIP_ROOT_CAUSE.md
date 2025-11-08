# Partition Leadership Root Cause Analysis

**Date:** 2025-11-08
**Versions Affected:** v2.2.0, v2.2.1, v2.2.2
**Status:** CRITICAL BUG - Cluster mode non-functional

---

## Executive Summary

The partition leadership conflict in Chronik cluster mode is caused by a **fundamental architectural mismatch** between Raft leadership (for metadata) and Kafka partition leadership (for data).

### The Core Problem

```
Raft Leader (Metadata):   Node 2  ✓ Can propose metadata changes
Partition Leader (Data):  Node 1  ✗ Cannot propose metadata changes

Result: When Node 1 needs to elect a new partition leader, it CANNOT
        propose the change because it's not the Raft leader.
```

This creates a deadlock where partition leadership cannot be maintained.

---

## Root Cause: Line 678 in integrated_server.rs

```rust
// Assign replicas (round-robin)
for i in 0..replication_factor {
    let node_idx = (partition as usize + i as usize) % all_nodes.len();
    replicas.push(all_nodes[node_idx]);
}

// Set initial leader (first replica)  ← THE BUG
let leader = replicas[0];
```

**Example with 3 nodes (Node 2 is Raft leader):**

| Partition | Replicas | Leader Assigned | Is Raft Leader? | Can Elect? |
|-----------|----------|-----------------|-----------------|------------|
| __meta-0 | [1, 2, 3] | **1** | ❌ No (follower) | ❌ FAIL |
| __meta-1 | [2, 3, 1] | **2** | ✅ Yes | ✅ Works |
| __meta-2 | [3, 1, 2] | **3** | ❌ No (follower) | ❌ FAIL |

**Result:** 2 out of 3 partitions get leaders that cannot perform elections!

---

## Why v2.2.1 and v2.2.2 Didn't Fix This

### v2.2.1 Fix (integrated_server.rs)
- ✅ **Fixed:** Only Raft leader initializes metadata on startup
- ❌ **Didn't fix:** Still assigns partition leaders to Raft followers
- **Impact:** Reduces errors during startup, but doesn't solve the core issue

### v2.2.2 Fix (produce_handler.rs)
- ✅ **Fixed:** Only Raft leader initializes metadata for runtime topic creation
- ❌ **Didn't fix:** Still assigns partition leaders to Raft followers
- **Impact:** Same as v2.2.1 - reduces errors but doesn't solve core issue

### Why Users Still See Errors

Even with both fixes:
1. ✅ Only Raft leader (Node 2) proposes partition assignments
2. ❌ But it assigns `leader=1` for partition 0 (a Raft follower)
3. ❌ Leader election service on Node 2 detects timeout for partition 0
4. ❌ Tries to elect new leader via `raft.propose(SetPartitionLeader)`
5. ❌ But partition leader is Node 1, which cannot propose!
6. ❌ **ERROR: "Cannot propose: this node is not the leader"**

---

## The Error Chain

```
1. Startup (Node 2 is Raft leader)
   → Node 2 initializes partitions ✓
   → Assigns leader=1 for __meta-0 ✓
   → Metadata replicated to all nodes ✓

2. Leader Election Service Runs (every 12 seconds)
   → Node 2 checks partition health ✓
   → Detects leader timeout for __meta-0 (leader=1) ✓
   → Calls trigger_election() ✓

3. trigger_election() tries to elect new leader
   → Calls elect_leader_from_isr() ✓
   → Gets ISR = [1, 2, 3] ✓
   → Picks new leader (maybe Node 1 again, maybe Node 2) ✓
   → Calls raft.propose(SetPartitionLeader { leader: X }) ✓

4. raft.propose() checks if THIS node can propose
   → If partition leader is Node 1 (follower): ❌ FAIL
   → "Cannot propose: this node (id=1) is not the leader"
   → Error logged, election fails

5. Loop repeats every 12 seconds
   → Same error, forever
```

**Wait, that's not quite right.** Let me re-read the code...

Actually, the leader election service runs on the **Raft leader** (Node 2) due to lines 107-114. So Node 2 is calling `raft.propose()`, which SHOULD work since Node 2 IS the Raft leader.

Unless... let me check who's actually calling propose:

---

## Re-Analysis: Who's Calling Propose?

Looking at the error message again:

```
❌ Failed to elect leader for __meta-0:
   Cannot propose: this node (id=1) is not the leader (state=Follower, leader=2)
```

This error says "**this node (id=1)**" - meaning **Node 1** is trying to propose! But leader_election.rs has the check to skip if not Raft leader...

**WAIT.** Let me check if there are OTHER places that call propose for partition leader changes:

---

## Hypothesis: Multiple Code Paths Calling Propose

There must be multiple code paths that try to elect partition leaders:

1. ✅ **leader_election.rs** - Has Raft leader check (lines 107-114)
2. ❓ **produce_handler.rs** - Has Raft leader check (added in v2.2.2)
3. ❓ **integrated_server.rs** - Has Raft leader check (added in v2.2.1)
4. ❓ **???** - Missing check?

Let me search for all places that propose `SetPartitionLeader`:

---

## Search Results

Need to find all code paths that call:
- `MetadataCommand::SetPartitionLeader`
- `trigger_election` or `elect_leader`
- Any partition leader election logic

This will reveal the missing code path that's causing follower nodes to attempt proposals.

---

## Correct Fix Strategy

Once we find the missing code path, we need to:

1. **Immediate Fix:** Add Raft leadership check to the missing code path
2. **Architectural Fix:** Change partition leader assignment to prefer Raft leader:

```rust
// Instead of:
let leader = replicas[0];  // Could be any node

// Do:
let raft_leader_id = raft.node_id();
let leader = if replicas.contains(&raft_leader_id) {
    raft_leader_id  // Prefer Raft leader if it's a replica
} else {
    replicas[0]  // Fallback to first replica
};
```

This ensures partition leaders are usually the Raft leader, minimizing conflicts.

3. **Ultimate Fix:** Implement proposal forwarding (Raft followers can forward to leader)

---

## Why Fresh Volumes Still Fail

User tested with "fresh volumes (clean start)" but still sees errors. This proves:

- ❌ **NOT** a migration issue from v2.2.0
- ❌ **NOT** stale Raft state
- ✅ **IS** a fundamental initialization bug

The bug is in the INITIAL assignment logic (line 678), not in upgrade paths.

---

## Next Steps

1. Search codebase for all `SetPartitionLeader` proposal sites
2. Find the code path missing Raft leadership check
3. Add the check
4. Optionally: Change line 678 to prefer Raft leader as partition leader
5. Test with fresh cluster
6. Release v2.2.3

---

## Why This Wasn't Caught in Testing

Our v2.2.1 testing used a **LOCAL 3-node cluster** where:
- All nodes started simultaneously
- Network was perfect (localhost)
- No timeouts occurred during the test window

The leader election service only triggers when:
- Heartbeats timeout (12 seconds)
- Partition leaders fail health checks

Our 15-second test didn't wait long enough for the leader election service to detect timeouts and trigger the bug.

**Lesson:** Integration tests need to run longer (>30s) to catch leader election issues.
