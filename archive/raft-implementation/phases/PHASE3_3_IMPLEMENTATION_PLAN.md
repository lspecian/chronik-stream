# Phase 3.3: ChronikMetaLog Replication via Raft - Implementation Plan

## Overview

This document describes the implementation plan for replicating metadata operations across a Chronik cluster using Raft consensus. The solution wraps the existing `ChronikMetaLogStore` with a Raft-backed layer to ensure metadata consistency.

## Architecture

### Current State (Single-Node)

```
Producer → ProduceHandler → WalProduceHandler → ChronikMetaLogStore (local WAL)
                                                         ↓
                                                   MetadataEvent
                                                         ↓
                                                   Local WAL append
```

### Target State (Clustered)

```
Producer → ProduceHandler → WalProduceHandler → RaftMetaLog (Raft wrapper)
                                                        ↓
                                                   Serialize MetadataEvent
                                                        ↓
                                                   Raft propose (replicate)
                                                        ↓
                                           (Wait for quorum commit)
                                                        ↓
                                             Apply to ChronikMetaLogStore
```

## Design Decisions

### 1. Single Raft Group for Metadata

**Decision**: Use one Raft group with partition key `("__meta", 0)` for all metadata operations.

**Rationale**:
- Metadata operations are low-volume compared to data writes
- Single group simplifies coordination
- All nodes have same metadata view
- Follows Kafka's controller pattern

### 2. Wrap, Don't Replace

**Decision**: `RaftMetaLog` wraps `ChronikMetaLogStore` instead of replacing it.

**Rationale**:
- Preserves existing WAL-based event sourcing
- Standalone mode still uses `ChronikMetaLogStore` directly
- Raft layer only activates in clustered mode
- Minimizes code changes and maintains backward compatibility

### 3. Serialization Format

**Decision**: Use `bincode` to serialize `MetadataEvent` for Raft entries.

**Rationale**:
- Matches existing storage layer serialization
- Compact binary format
- Fast ser/de
- Already used for `CanonicalRecord` in data replication

### 4. Read Path

**Decision**: Reads query local state (no Raft coordination).

**Rationale**:
- Metadata reads are much more frequent than writes
- Local reads provide low latency
- State is eventually consistent after Raft commit
- Matches Raft best practices (linearizable writes, stale reads)

### 5. Write Path

**Decision**: All writes go through Raft `propose_and_wait()`.

**Rationale**:
- Ensures strong consistency
- Blocks until quorum commit
- Metadata operations are infrequent (acceptable latency)
- Prevents split-brain scenarios

## Implementation

### File: `crates/chronik-common/src/metadata/raft_meta_log.rs`

**Structure**:
```rust
pub struct RaftMetaLog {
    /// Underlying metadata store (applies committed entries)
    inner: Arc<ChronikMetaLogStore>,

    /// Raft replica manager for proposing operations
    raft_manager: Arc<RaftReplicaManager>,

    /// This node's ID
    node_id: u64,
}
```

**Key Methods**:
1. `new()` - Create with inner store and Raft manager
2. `propose_event()` - Serialize and propose metadata event via Raft
3. `apply_event()` - Apply committed event to inner store
4. All `MetadataStore` trait methods - wrap with Raft propose logic

### File: `crates/chronik-common/src/metadata/raft_state_machine.rs`

**Purpose**: Raft state machine that applies committed metadata events.

**Structure**:
```rust
pub struct MetadataStateMachine {
    /// Reference to the inner metadata store
    inner: Arc<ChronikMetaLogStore>,
}

#[async_trait]
impl StateMachine for MetadataStateMachine {
    async fn apply(&mut self, entry: &RaftEntry) -> Result<Bytes> {
        // Deserialize MetadataEvent from entry.data
        let event: MetadataEvent = bincode::deserialize(&entry.data)?;

        // Apply to inner store (bypassing Raft layer)
        self.inner.apply_event_direct(event).await?;

        Ok(Bytes::from("applied"))
    }

    // snapshot(), restore(), last_applied()
}
```

### Integration with `IntegratedKafkaServer`

**Location**: `crates/chronik-server/src/integrated_server.rs`

**Changes**:
1. Detect if clustering is enabled (`config.cluster_config.is_some()`)
2. If clustered:
   - Create `ChronikMetaLogStore` (inner)
   - Create metadata Raft replica: `("__meta", 0)` partition
   - Create `RaftMetaLog` wrapper
   - Use `RaftMetaLog` as metadata store
3. If standalone:
   - Use `ChronikMetaLogStore` directly (no Raft overhead)

**Pseudocode**:
```rust
let metadata_store: Arc<dyn MetadataStore> = if let Some(cluster_config) = &config.cluster_config {
    // CLUSTERED MODE: Use Raft replication
    info!("Initializing Raft-replicated metadata store");

    // Create inner store
    let inner = Arc::new(ChronikMetaLogStore::new(...).await?);

    // Create metadata Raft group
    let log_storage = create_raft_log_storage(
        Path::new(&config.data_dir),
        "__meta",
        0
    ).await?;

    let peers = cluster_config.nodes.iter()
        .map(|n| n.node_id)
        .filter(|id| *id != config.node_id)
        .collect();

    raft_manager.create_replica(
        "__meta".to_string(),
        0,
        log_storage,
        peers,
    ).await?;

    // Wrap with RaftMetaLog
    Arc::new(RaftMetaLog::new(inner, raft_manager, config.node_id)) as Arc<dyn MetadataStore>
} else {
    // STANDALONE MODE: Direct ChronikMetaLogStore
    info!("Initializing local metadata store (standalone)");
    Arc::new(ChronikMetaLogStore::new(...).await?) as Arc<dyn MetadataStore>
};
```

## Data Flow Example: Create Topic

### Request Flow (Leader Node)

1. Client sends `CreateTopics` Kafka request
2. `ProduceHandler::create_topic()` calls `metadata_store.create_topic()`
3. `RaftMetaLog::create_topic()`:
   - Creates `MetadataEventPayload::TopicCreated`
   - Serializes to `bincode`
   - Calls `raft_manager.propose("__meta", 0, data).await`
4. Raft replicates to followers
5. Quorum commits
6. `MetadataStateMachine::apply()` on all nodes:
   - Deserializes `MetadataEvent`
   - Calls `inner.apply_event_direct(event)`
   - Updates local metadata state
7. Leader returns success to client

### Follower Nodes

1. Receive Raft message from leader
2. Append to local Raft log
3. Send acknowledgment
4. After commit:
   - `MetadataStateMachine::apply()` invoked
   - Metadata updated locally
5. Future reads see the new topic

## Testing Strategy

### Unit Tests

**Location**: `crates/chronik-common/src/metadata/raft_meta_log.rs`

1. `test_propose_event()` - Verify serialization and Raft propose
2. `test_apply_event()` - Verify state machine application
3. `test_read_path()` - Verify reads query local state

### Integration Tests

**Location**: `tests/integration/raft_metadata_replication.rs`

1. `test_single_node_metadata()` - Verify standalone mode still works
2. `test_3node_topic_create()` - Create topic on leader, verify on followers
3. `test_metadata_recovery()` - Kill leader, verify new leader has all metadata
4. `test_partition_assignment()` - Test partition assignment replication
5. `test_consumer_group_replication()` - Test consumer group state replication

## Rollout Plan

### Phase 1: Implementation
- Implement `RaftMetaLog` wrapper
- Implement `MetadataStateMachine`
- Add unit tests

### Phase 2: Integration
- Integrate with `IntegratedKafkaServer`
- Add cluster config detection
- Preserve standalone mode

### Phase 3: Testing
- Run integration tests
- Verify 3-node cluster metadata sync
- Test failure scenarios

### Phase 4: Validation
- Deploy test cluster
- Create topics via Kafka clients
- Verify metadata consistency across nodes
- Test leader failover

## Success Criteria

1. ✅ Metadata operations replicate across all nodes
2. ✅ Topic created on leader visible on all followers
3. ✅ Consumer offsets replicate correctly
4. ✅ Standalone mode unchanged (no Raft overhead)
5. ✅ Leader failover preserves metadata state
6. ✅ All existing tests pass

## Risks and Mitigations

### Risk 1: Raft Latency on Metadata Operations

**Mitigation**: Metadata operations are infrequent. Acceptable latency tradeoff for consistency.

### Risk 2: Single Raft Group Bottleneck

**Mitigation**: Metadata volume is low. Can shard later if needed (unlikely).

### Risk 3: State Machine Bugs

**Mitigation**: Extensive testing. Reuse existing `ChronikMetaLogStore` logic.

### Risk 4: Backward Compatibility

**Mitigation**: Standalone mode uses existing path. Raft only activates in clustered mode.

## Future Enhancements

1. **Read-Only Replicas**: Allow non-voting followers for read scaling
2. **Metadata Snapshots**: Compress metadata history for faster recovery
3. **Multi-Raft Groups**: Shard metadata by topic prefix if needed
4. **Lease-Based Reads**: Add lease mechanism for linearizable reads (optional)

## Dependencies

- `chronik-common` (metadata abstractions)
- `chronik-raft` (Raft implementation)
- `chronik-server` (integration point)
- `chronik-wal` (underlying storage)

## Timeline

- **Day 1**: Implement `RaftMetaLog` and `MetadataStateMachine`
- **Day 2**: Integrate with `IntegratedKafkaServer`
- **Day 3**: Write integration tests
- **Day 4**: Testing and validation
- **Day 5**: Documentation and review

## References

- Raft Paper: https://raft.github.io/raft.pdf
- etcd Raft: https://github.com/etcd-io/etcd/tree/main/raft
- TiKV Multi-Raft: https://tikv.org/deep-dive/scalability/multi-raft/
- Kafka Controller: https://kafka.apache.org/documentation/#controller
