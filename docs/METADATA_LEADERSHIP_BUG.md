# Partition Leadership Bug - Complete Root Cause Analysis

## Executive Summary

**Issue**: acks=all works for single-partition topics but times out for multi-partition topics.

**Root Cause**: `produce_handler.rs` incorrectly checks if current node is the **Raft cluster leader** instead of checking if it's the **partition leader** (first replica in replica list).

**Impact**: When a producer connects to a node that is NOT the partition leader, the non-leader node incorrectly accepts the produce request, tries to use WAL replication with acks=-1, but never receives ACKs because it's not actually the partition leader.

## Problem Description

### What Works ✅
- **Single-partition topic**: `test-metadata-fix` with replicas=[1, 2, 3]
  - Node 1 is Raft leader AND partition leader
  - Producer → Node 1 → WAL replication → ACKs received → Success

### What Fails ❌
- **Multi-partition topic**: `test-acks-multiple` with 3 partitions
  - Partition 0: replicas=[1, 2, 3] - Leader: Node 1
  - Partition 1: replicas=[2, 3, 1] - Leader: Node 2  ← **THIS IS THE PROBLEM**
  - Partition 2: replicas=[3, 1, 2] - Leader: Node 3

  When producer sends to localhost:9092 (Node 1), Kafka client round-robins messages across partitions. Messages to partition 1 go to Node 1, but **Node 1 is NOT the partition leader**.

## Evidence

### Log Analysis

**Node 1 log shows incorrect leadership check:**
```
[DEBUG] Raft leadership check for test-acks-multiple-1: node_id=1, leader=1, is_leader=true
```

This is WRONG! Node 1 is the Raft cluster leader, but it's NOT the leader for partition 1. The partition leader for partition 1 is Node 2 (first in replicas=[2, 3, 1]).

**What should happen:**
1. Producer sends to Node 1 for partition 1
2. Node 1 checks if it's the partition leader
3. Node 1 sees replicas=[2, 3, 1], leader is Node 2
4. Node 1 returns `NOT_LEADER_FOR_PARTITION` error with leader_hint=2
5. Producer redirects to Node 2 (localhost:9093)
6. Node 2 (partition leader) handles the produce request with acks=-1

**What actually happens:**
1. Producer sends to Node 1 for partition 1
2. Node 1 checks if it's the **Raft leader** (YES) ← WRONG CHECK
3. Node 1 thinks it's the partition leader and accepts request
4. Node 1 registers acks=-1 wait in IsrAckTracker
5. Node 1 tries to do WAL replication
6. **BUT** Node 1 is not in partition_followers for partition 1 (only Node 2's followers are registered)
7. No WAL frames sent, no ACKs received
8. Request times out after 10 seconds ❌

### Key Code Location

**File**: `crates/chronik-server/src/produce_handler.rs`
**Lines**: 1061-1094

```rust
let (is_leader, leader_hint) = {
    if let Some(ref raft_cluster) = raft_cluster {
        // Get partition leader from Raft metadata state machine
        let leader_id = raft_cluster.get_partition_leader(&topic_name, partition_data.index);

        if let Some(leader) = leader_id {
            // Partition is managed by Raft
            let is_raft_leader = leader == raft_cluster.node_id();  // ← BUG HERE

            debug!(
                "Raft leadership check for {}-{}: node_id={}, leader={}, is_leader={}",
                topic_name, partition_data.index, raft_cluster.node_id(), leader, is_raft_leader
            );

            (is_raft_leader, Some(leader))  // ← RETURNS WRONG VALUE
```

**The Bug**: Line 1068 compares `leader` (which is the partition leader from metadata) with `raft_cluster.node_id()` (which is the Raft cluster leader).

**What `leader_id` actually is**: Looking at the metadata replication logs, we see:
```
AssignPartition { topic: "test-acks-multiple", partition: 1, replicas: [2, 3, 1] }
```

The replicas list determines the partition leader (first element). So for partition 1, `replicas=[2, 3, 1]` means the partition leader is Node 2.

## Root Cause Analysis

### The Confusion

Chronik has TWO separate concepts of leadership:

1. **Raft Cluster Leadership** (for metadata consensus)
   - ONE leader for the entire cluster
   - Determined by Raft election
   - Purpose: Metadata operations (create topic, assign partitions)
   - In our test: Node 1 is the Raft leader

2. **Partition Leadership** (for data replication)
   - SEPARATE leader for each partition
   - Determined by partition assignment (first replica)
   - Purpose: Data WAL replication and ISR coordination
   - In our test:
     - Partition 0 leader: Node 1
     - Partition 1 leader: Node 2
     - Partition 2 leader: Node 3

### Why It Worked for Single Partition

For `test-metadata-fix`:
- Replicas: [1, 2, 3]
- Partition leader: Node 1 (first in list)
- Raft leader: Node 1
- Node 1 is BOTH Raft leader AND partition leader → check passes correctly by coincidence

### Why It Fails for Multi-Partition

For `test-acks-multiple`:
- Partition 1 replicas: [2, 3, 1]
- Partition 1 leader: Node 2 (first in list)
- Raft leader: Node 1
- Producer sends to Node 1 (localhost:9092)
- Node 1 checks: "Am I Raft leader?" YES ← WRONG!
- Node 1 accepts request for partition it doesn't lead
- No WAL replication happens (Node 1 isn't registered as partition leader)
- Request times out

## The Correct Implementation

`raft_cluster.get_partition_leader()` **ALREADY returns the correct partition leader**! The bug is that we're comparing it with the wrong value.

**Current (broken) code:**
```rust
let leader_id = raft_cluster.get_partition_leader(&topic_name, partition_data.index);
//     ^^^^^^^ This returns the PARTITION leader (e.g., Node 2 for partition 1)

if let Some(leader) = leader_id {
    let is_raft_leader = leader == raft_cluster.node_id();
    //                   ^^^^^^    ^^^^^^^^^^^^^^^^^^^
    //                   Node 2  ==  Node 1  → FALSE (correct!)
    //                   But variable name "is_raft_leader" is confusing!
```

Wait... actually looking at this more carefully, the comparison `leader == raft_cluster.node_id()` should work! If `leader` is 2 (partition leader) and `raft_cluster.node_id()` is 1 (current node), then `is_raft_leader` would be FALSE, which is correct.

Let me check what `raft_cluster.get_partition_leader()` actually returns...

## Investigation: What Does get_partition_leader() Return?

From the logs:
```
Raft leadership check for test-acks-multiple-1: node_id=1, leader=1, is_leader=true
```

This shows:
- `node_id=1` (current node is Node 1)
- `leader=1` (partition leader is Node 1???)  ← THIS IS WRONG!
- `is_leader=true` (comparison result)

So `get_partition_leader()` is returning Node 1 as the partition leader for partition 1, when it should return Node 2!

**Real Root Cause**: `get_partition_leader()` is returning the wrong value. It's returning the Raft leader instead of looking up the partition leader from the metadata.

##What get_partition_leader() Should Do

From `raft_cluster.rs:336`:
```rust
pub fn get_partition_leader(&self, topic: &str, partition: i32) -> Option<u64> {
    let sm = self.state_machine.read().ok()?;
    sm.get_partition_leader(topic, partition)
}
```

This calls the state machine's `get_partition_leader()`, which should look up the partition metadata and return the first element of the replicas list.

**Hypothesis**: The state machine's `get_partition_leader()` implementation is buggy or the partition metadata is not being properly updated on followers.

## Next Steps

1. ✅ Document root cause (this file)
2. ⏳ Find the state machine's `get_partition_leader()` implementation
3. ⏳ Check if partition metadata is correctly stored in the state machine
4. ⏳ Fix the implementation to return the first replica as partition leader
5. ⏳ Test with multi-partition topics
