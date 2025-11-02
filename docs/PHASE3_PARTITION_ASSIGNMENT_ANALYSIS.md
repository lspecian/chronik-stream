# Phase 3: Partition Assignment via Raft - Analysis

**Date**: 2025-11-01
**Version**: v2.5.0
**Status**: Partially Implemented

## Executive Summary

Phase 3 implementation for Raft-based partition assignment is **90% complete** but has a critical gap:

✅ **WORKING**: Auto-created topics (via produce) get Raft partition assignments
✗ **MISSING**: Explicitly created topics (via CreateTopics API) bypass Raft

## What Works

### 1. ProduceHandler.auto_create_topic() - ✅ COMPLETE

**Location**: [crates/chronik-server/src/produce_handler.rs:2120-2238](crates/chronik-server/src/produce_handler.rs#L2120-L2238)

When topics are auto-created during produce operations, the following Raft commands are proposed:

```rust
// Lines 2185-2237
if let Some(ref raft) = self.raft_cluster {
    for partition in 0..self.config.num_partitions {
        // 1. Assign replicas (round-robin)
        raft.propose(MetadataCommand::AssignPartition {
            topic: topic_name.to_string(),
            partition: partition as i32,
            replicas: replicas.clone(),
        }).await?;

        // 2. Set partition leader
        raft.propose(MetadataCommand::SetPartitionLeader {
            topic: topic_name.to_string(),
            partition: partition as i32,
            leader: replicas[0],
        }).await?;

        // 3. Set initial ISR
        raft.propose(MetadataCommand::UpdateISR {
            topic: topic_name.to_string(),
            partition: partition as i32,
            isr: replicas.clone(),
        }).await?;
    }
}
```

**Tested**: ✅ Works correctly for auto-created topics

### 2. MetadataStateMachine - ✅ COMPLETE

**Location**: [crates/chronik-server/src/raft_metadata.rs:24-138](crates/chronik-server/src/raft_metadata.rs#L24-L138)

The Raft state machine correctly handles:
- `AssignPartition` - Stores partition → replica mappings
- `SetPartitionLeader` - Tracks partition leaders
- `UpdateISR` - Maintains in-sync replica sets

**Tested**: ✅ State machine applies commands correctly

### 3. RaftCluster - ✅ COMPLETE

**Location**: [crates/chronik-server/src/raft_cluster.rs:206-232](crates/chronik-server/src/raft_cluster.rs#L206-L232)

Provides query APIs for partition metadata:
- `get_partition_replicas(topic, partition)` - Returns replica node IDs
- `get_partition_leader(topic, partition)` - Returns leader node ID
- `get_isr(topic, partition)` - Returns in-sync replicas

**Tested**: ✅ Query methods work correctly

## What's Missing

### 4. CreateTopics API Handler - ✗ INCOMPLETE

**Location**: [crates/chronik-protocol/src/handler.rs:3484-3680](crates/chronik-protocol/src/handler.rs#L3484-L3680)

**Problem**: The `handle_create_topics()` method creates topics directly in the metadata store WITHOUT proposing partition assignments to Raft:

```rust
// Lines 6544-6550 - Creates local partition assignments only
for partition in 0..topic.num_partitions {
    let broker_index = (partition as usize) % broker_count;
    let assignment = PartitionAssignment {
        topic: topic_name.to_string(),
        partition,
        broker_id: broker_id,
        is_leader: true,
    };
    // ❌ Only stores locally, NOT proposed to Raft!
    metadata_store.assign_partition(assignment).await?;
}
```

**Impact**:
- Topics created via `kafka-topics --create` or `KafkaAdminClient.create_topics()` only exist on one node
- Partition assignments are NOT replicated across the cluster
- Metadata is inconsistent between nodes

**Evidence from Test Run**:
```
✓ Topic visible on port 9092  (Node 1 - where CreateTopics was sent)
✗ Topic NOT visible on port 9093  (Node 2 - follower)
✗ Topic NOT visible on port 9094  (Node 3 - follower)

WARN: DEBUG_TRACE: Using non-Raft path for test-raft-partitions-0
```

## Root Cause Analysis

### Architecture Issue

The problem is a **layering violation**:

```
┌──────────────────────────────────────────────────┐
│  chronik-protocol                                │
│  └─ ProtocolHandler (has MetadataStore)         │
│     └─ handle_create_topics()                   │
│        ❌ Cannot access RaftCluster              │
└──────────────────────────────────────────────────┘
                       ↓
┌──────────────────────────────────────────────────┐
│  chronik-server                                  │
│  ├─ ProduceHandler (has RaftCluster)            │
│  │  └─ auto_create_topic() ✅ Uses Raft         │
│  └─ KafkaProtocolHandler (wires protocol layer) │
└──────────────────────────────────────────────────┘
```

`ProtocolHandler` lives in `chronik-protocol` crate and doesn't have access to `RaftCluster`, which is in `chronik-server`.

## Solution Options

### Option 1: Pass RaftCluster to ProtocolHandler (Recommended)

**Pros**:
- Clean architectural solution
- ProtocolHandler can directly propose Raft commands
- Consistent with how ProduceHandler works

**Cons**:
- Requires passing RaftCluster through multiple layers
- Need to update ProtocolHandler constructor

**Implementation**:
1. Add `raft_cluster: Option<Arc<RaftCluster>>` to `ProtocolHandler` struct
2. Update `ProtocolHandler::new()` to accept RaftCluster
3. Wire it through `KafkaProtocolHandler` and `IntegratedKafkaServer`
4. In `handle_create_topics()`, call `raft.propose()` for each partition

### Option 2: Delegate to ProduceHandler

**Pros**:
- Minimal changes
- Reuses existing `auto_create_topic()` logic

**Cons**:
- Weird layering (protocol handler calling produce handler)
- Would need to pass ProduceHandler to ProtocolHandler

**Implementation**:
1. Pass `ProduceHandler` to `ProtocolHandler`
2. In `handle_create_topics()`, call `produce_handler.auto_create_topic()`
3. Remove duplicate partition assignment logic

### Option 3: Create a TopicManager

**Pros**:
- Best long-term architecture
- Single responsibility for topic lifecycle
- Can be used by both ProtocolHandler and ProduceHandler

**Cons**:
- Most work
- New component to maintain

**Implementation**:
1. Create `TopicManager` with `create_topic(name, config, raft_cluster)`
2. Use from both `ProtocolHandler` and `ProduceHandler`
3. Centralizes all topic creation logic

## Recommended Fix (Option 1 + 2 Hybrid)

**Short-term** (Quick fix for Phase 3):
1. Pass `ProduceHandler` to `ProtocolHandler` constructor
2. In `handle_create_topics()`, after creating topic in metadata store:
   ```rust
   if let Some(produce_handler) = &self.produce_handler {
       // Let ProduceHandler handle Raft partition assignment
       produce_handler.initialize_raft_partitions(topic_name, num_partitions).await?;
   }
   ```
3. Add `ProduceHandler::initialize_raft_partitions()` helper method

**Long-term** (Future refactor):
- Extract topic creation logic to dedicated `TopicManager`
- Make it the single source of truth for topic lifecycle

## Testing Plan

After fix:
1. Start 3-node Raft cluster
2. Create topic via `kafka-topics --create` on Node 1
3. Verify logs show `AssignPartition`, `SetPartitionLeader`, `UpdateISR` commands
4. Verify topic is visible on all nodes (9092, 9093, 9094)
5. Verify metadata is consistent across cluster
6. Test produce/consume from any node

## Conclusion

**Phase 3 Status**: 90% Complete

✅ Infrastructure is ready (Raft state machine, RaftCluster, propose methods)
✅ Auto-created topics work correctly
✗ Explicit topic creation bypasses Raft (needs wiring fix)

**Estimated fix time**: 2-4 hours (Option 1+2 hybrid approach)

**Risk level**: Low - existing Raft code is solid, just needs wiring

---

**Next Steps**:
1. Implement Option 1+2 hybrid fix
2. Test with multi-node cluster
3. Verify metadata consistency
4. Move to Phase 4 (leader checks in ProduceHandler)
