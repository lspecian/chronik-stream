# Phase 2 Data Flow Diagram

**Version**: v2.3.0 | **Date**: 2025-11-11

---

## Complete Data Flow: Topic Creation with Phase 2

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              KAFKA CLIENT                                   │
│                    (kafka-python, Java client, KSQL, etc.)                  │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ Kafka Wire Protocol
                                    │ CreateTopics Request
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                           LEADER NODE (Node 1)                              │
│                         Port 9092 - Kafka Protocol                          │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. KafkaHandler::handle_request()                                          │
│     ├─ Parse ApiKey::CreateTopics                                           │
│     └─ Route to RaftMetadataStore::create_topic()                           │
│                                                                             │
│  2. RaftMetadataStore::create_topic()                                       │
│     ├─ Check: metadata_wal.is_some() && am_i_leader() ?                     │
│     │   ✅ YES → PHASE 2 FAST PATH                                          │
│     │   ❌ NO  → Phase 1 Raft fallback                                      │
│     │                                                                        │
│     └─ FAST PATH:                                                           │
│         │                                                                    │
│         │  Step 1: Create MetadataCommand                                   │
│         │  ┌────────────────────────────────────────┐                       │
│         │  │ MetadataCommand::CreateTopic {         │                       │
│         │  │   name: "my-topic",                    │                       │
│         │  │   partition_count: 3,                  │                       │
│         │  │   replication_factor: 2,               │                       │
│         │  │   config: { ... }                      │                       │
│         │  │ }                                       │                       │
│         │  └────────────────────────────────────────┘                       │
│         │                                                                    │
│         ▼                                                                    │
│                                                                             │
│  3. MetadataWal::append(cmd)                           ⏱️  1-2ms            │
│     ├─ Serialize to bincode: Vec<u8>                                        │
│     ├─ Allocate offset: AtomicI64::fetch_add(1)                             │
│     │   → offset = 0                                                         │
│     ├─ Create WalRecord::new_v2(                                            │
│     │     topic: "__chronik_metadata",                                      │
│     │     partition: 0,                                                     │
│     │     data: bincode_bytes,                                              │
│     │     base_offset: 0,                                                   │
│     │     last_offset: 0,                                                   │
│     │     record_count: 1                                                   │
│     │   )                                                                    │
│     ├─ GroupCommitWal::append(record, acks=1)                               │
│     │   → fsync to disk (durable!)                                          │
│     └─ Return offset: 0                                                     │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  4. RaftCluster::apply_metadata_command_direct(cmd)    ⏱️  <1ms            │
│     ├─ Lock state machine: state_machine.write()                            │
│     ├─ Apply to in-memory state:                                            │
│     │   topics.insert("my-topic", TopicMetadata { ... })                    │
│     └─ Release lock                                                         │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  5. Fire notification (wake waiting threads)           ⏱️  <1ms            │
│     └─ pending_topics.remove("my-topic").notify_waiters()                   │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  6. Return success to client                           ⏱️  TOTAL: 1-2ms    │
│     ├─ CreateTopicsResponse { ... }                                         │
│     └─ Client receives success                                              │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  7. Spawn async replication task (FIRE-AND-FORGET)     ⏱️  Non-blocking    │
│     └─ tokio::spawn(async move {                                            │
│            replicator.replicate(cmd, offset).await;                         │
│        });                                                                  │
│         │                                                                    │
│         └──────────────────────────────┐                                    │
│                                        │                                    │
└────────────────────────────────────────┼────────────────────────────────────┘
                                        │
                                        │ Async Replication
                                        │ (happens in background)
                                        │
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                     MetadataWalReplicator::replicate()                      │
│                      (Still on Leader, Background Task)                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. Serialize command                                                       │
│     └─ bincode::serialize(cmd) → Vec<u8>                                    │
│                                                                             │
│  2. WalReplicationManager::replicate_partition()                            │
│     └─ Send to ALL followers:                                               │
│         ├─ topic: "__chronik_metadata"                                      │
│         ├─ partition: 0                                                     │
│         ├─ offset: 0                                                        │
│         ├─ data: bincode_bytes                                              │
│         └─ transport: Existing TCP on port 9291                             │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                        │
                                        │ TCP Stream (port 9291)
                                        │ WalReplicationRecord
                                        │
                    ┌───────────────────┴───────────────────┐
                    │                                       │
                    ▼                                       ▼
        ┌───────────────────────┐             ┌───────────────────────┐
        │   FOLLOWER NODE 2     │             │   FOLLOWER NODE 3     │
        │   Port 9291 Receiver  │             │   Port 9291 Receiver  │
        └───────────────────────┘             └───────────────────────┘
                    │                                       │
                    └───────────────────┬───────────────────┘
                                        │
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         FOLLOWER NODE (Node 2/3)                            │
│                       Port 9291 - WAL Replication                           │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. WalReceiver::handle_connection()                                        │
│     ├─ Read WalReplicationRecord from TCP stream                            │
│     │   ├─ topic: "__chronik_metadata"                                      │
│     │   ├─ partition: 0                                                     │
│     │   ├─ offset: 0                                                        │
│     │   └─ data: Vec<u8>                                                    │
│     │                                                                        │
│     └─ Detect special topic: "__chronik_metadata"                           │
│         ✅ Metadata replication detected!                                   │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  2. WalReceiver::handle_metadata_wal_record()          ⏱️  <1ms            │
│     ├─ Deserialize command:                                                 │
│     │   bincode::deserialize::<MetadataCommand>(data)                       │
│     │   → CreateTopic { name: "my-topic", ... }                             │
│     │                                                                        │
│     └─ Log: "Phase 2.3: Follower received metadata replication"             │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  3. RaftCluster::apply_metadata_command_direct(cmd)    ⏱️  <1ms            │
│     ├─ Lock state machine: state_machine.write()                            │
│     ├─ Apply to in-memory state:                                            │
│     │   topics.insert("my-topic", TopicMetadata { ... })                    │
│     │   ✅ Follower now has same state as leader!                           │
│     └─ Release lock                                                         │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  4. Fire notification (wake waiting threads)           ⏱️  <1ms            │
│     └─ pending_topics.remove("my-topic").notify_waiters()                   │
│         (If any client was waiting on this follower)                        │
│                                                                             │
│         ▼                                                                    │
│                                                                             │
│  5. Log success                                        ⏱️  TOTAL: <1ms     │
│     └─ info!("METADATA✓ Replicated: __chronik_metadata-0 offset 0")        │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Latency Breakdown

### Leader Node (Client Perspective)

| Step | Operation | Latency | Cumulative |
|------|-----------|---------|------------|
| 1 | Parse request | < 0.1ms | 0.1ms |
| 2 | Check Phase 2 enabled | < 0.01ms | 0.11ms |
| 3 | **Write to metadata WAL** | **1-2ms** | **1.11-2.11ms** |
| 4 | Apply to state machine | < 0.1ms | 1.21-2.21ms |
| 5 | Fire notification | < 0.01ms | 1.22-2.22ms |
| 6 | Serialize response | < 0.1ms | 1.32-2.32ms |
| 7 | Send response to client | < 0.5ms | **1.82-2.82ms** |

**Total client-visible latency**: **~2-3ms** (vs 10-50ms with Raft)

### Follower Node (Background)

| Step | Operation | Latency | Cumulative |
|------|-----------|---------|------------|
| 1 | Receive TCP data | < 0.5ms | 0.5ms |
| 2 | Deserialize command | < 0.1ms | 0.6ms |
| 3 | Apply to state machine | < 0.1ms | 0.7ms |
| 4 | Fire notification | < 0.01ms | 0.71ms |
| 5 | Log success | < 0.01ms | **0.72ms** |

**Total follower replication latency**: **<1ms** (happens async, doesn't affect client)

---

## Phase 1 vs Phase 2 Comparison

### Phase 1 (Raft Consensus) - OLD

```
Client → Leader → Raft Propose → Wait for Quorum (2/3 nodes) → Apply → Response
         ⏱️ 10-50ms total (network RTTs + consensus + disk I/O)
```

**Breakdown**:
- Propose to followers: ~2-5ms (network)
- Follower WAL write: ~1-2ms
- Follower response: ~2-5ms (network)
- Leader apply: ~0.1ms
- **Total**: ~10-50ms

### Phase 2 (WAL-Based) - NEW

```
Client → Leader → WAL Write → Apply → Response
         ⏱️ 1-2ms total (local disk I/O only)

         (Async) → Replicate to followers → Apply on followers
                   ⏱️ happens in background, non-blocking
```

**Breakdown**:
- WAL write (local): ~1-2ms
- Apply (local): ~0.1ms
- Response: ~0.5ms
- **Total**: ~1-2ms

**Speedup**: **5-25x faster** ⚡

---

## Wire Protocol Details

### Metadata WAL Record Format (on port 9291)

```
┌─────────────────────────────────────────────────────────┐
│              WalReplicationRecord                       │
├─────────────────────────────────────────────────────────┤
│ topic: String = "__chronik_metadata"                    │
│ partition: i32 = 0                                      │
│ offset: i64 = 0, 1, 2, ...                              │
│ leader_high_watermark: i64 = offset                     │
│ data: Vec<u8> = bincode::serialize(MetadataCommand)     │
│   │                                                      │
│   └─> MetadataCommand (bincode serialized):             │
│       ┌─────────────────────────────────────────┐       │
│       │ enum MetadataCommand {                  │       │
│       │   CreateTopic {                         │       │
│       │     name: String,                       │       │
│       │     partition_count: i32,               │       │
│       │     replication_factor: i32,            │       │
│       │     config: HashMap<String, String>,    │       │
│       │   },                                     │       │
│       │   RegisterBroker { ... },               │       │
│       │   // ... other commands                 │       │
│       │ }                                        │       │
│       └─────────────────────────────────────────┘       │
└─────────────────────────────────────────────────────────┘
```

### Detection on Follower

```rust
// In WalReceiver::handle_connection()
if wal_record.topic == "__chronik_metadata" && wal_record.partition == 0 {
    // ✅ This is metadata replication, handle specially
    Self::handle_metadata_wal_record(raft_cluster, &wal_record).await?;
    info!("METADATA✓ Replicated: {}-{} offset {}", topic, partition, offset);
    continue;  // Skip normal WAL write
}

// Normal partition data handled below...
```

---

## Error Handling and Edge Cases

### Case 1: Follower Receives Metadata Before Leader Election

**Scenario**: Follower receives metadata replication, but doesn't know who the leader is yet

**Handling**:
```rust
if let Some(ref raft) = raft_cluster {
    // Apply directly
    raft.apply_metadata_command_direct(cmd)?;
} else {
    // No raft_cluster configured, log warning
    warn!("Received metadata WAL replication but no RaftCluster configured");
}
```

**Outcome**: Safe to apply, metadata is append-only and idempotent

---

### Case 2: Leader Crashes After WAL Write, Before Replication

**Scenario**: Leader writes to WAL, crashes before replicating

**Handling**:
1. WAL write is durable (persisted to disk)
2. On leader recovery, WAL replay restores metadata
3. New leader election occurs (Raft)
4. New leader may re-create topic (idempotent operation)

**Outcome**: At-least-once semantics, safe for metadata operations

---

### Case 3: Network Partition Between Leader and Followers

**Scenario**: Leader can't reach followers for replication

**Handling**:
1. Leader returns success to client (WAL write succeeded)
2. Replication task logs warning: "Metadata replication failed"
3. Followers eventually reconnect
4. Catch-up happens via:
   - Option A: WAL replay (if follower was down)
   - Option B: Raft snapshot (if follower falls too far behind)

**Outcome**: Eventual consistency, acceptable for metadata

---

## Performance Metrics to Monitor

### Leader Metrics

```rust
// Add these metrics in RaftMetadataStore
gauge!("phase2.metadata_wal.offset", wal_offset as f64);
histogram!("phase2.create_topic.latency_ms", latency_ms);
counter!("phase2.fast_path.count", 1);
counter!("phase2.raft_fallback.count", 0);  // Should be 0 for leaders
```

**Expected values** (Phase 2 active):
- `create_topic.latency_ms` p99: < 5ms
- `fast_path.count`: > 95% of requests
- `raft_fallback.count`: 0 (for leaders)

### Follower Metrics

```rust
// Add these metrics in WalReceiver
counter!("phase2.metadata_replicated.count", 1);
histogram!("phase2.metadata_apply.latency_ms", latency_ms);
```

**Expected values**:
- `metadata_replicated.count`: Should match leader's fast_path.count
- `metadata_apply.latency_ms` p99: < 1ms

---

## Log Queries for Debugging

### Verify Phase 2 Activation

```bash
# On leader (Node 1)
grep "Phase 2: Metadata WAL enabled" logs/node1.log
grep "Phase 2: RaftMetadataStore initialized" logs/node1.log

# On followers (Node 2/3)
grep "Phase 2.3: WalReceiver configured" logs/node2.log
grep "Phase 2.3: WalReceiver configured" logs/node3.log
```

### Verify Fast Path Usage

```bash
# On leader - should see these for every topic creation
grep "Phase 2: Leader creating topic" logs/node1.log
grep "Wrote.*to metadata WAL at offset" logs/node1.log

# Should NOT see these (fallback)
grep "Phase 1 fallback" logs/node1.log  # Should be empty
```

### Verify Follower Replication

```bash
# On followers - should see these for every replicated metadata op
grep "METADATA✓ Replicated" logs/node2.log
grep "Phase 2.3: Follower received metadata replication" logs/node2.log
grep "Phase 2.3: Follower applied replicated metadata command" logs/node2.log
```

---

## References

- [PHASE2_INTEGRATION_GUIDE.md](PHASE2_INTEGRATION_GUIDE.md) - Integration steps
- [PHASE2_VERIFICATION_CHECKLIST.md](PHASE2_VERIFICATION_CHECKLIST.md) - Testing checklist
- [PHASE2_QUICK_REFERENCE.md](PHASE2_QUICK_REFERENCE.md) - Quick reference
- [PHASE2_IMPLEMENTATION_SUMMARY.md](PHASE2_IMPLEMENTATION_SUMMARY.md) - Implementation details

---

**Status**: Phase 2 implementation complete ✅ | Ready for integration 🚀
