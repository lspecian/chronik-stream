# Metadata Split-Brain Root Cause Analysis

**Date:** 2025-11-08
**Version:** v2.2.6 (current)
**Issue:** `__meta` topic appears on follower nodes with incorrect leadership
**Severity:** CRITICAL - Affects cluster metadata consistency

## Executive Summary

Chronik cluster has a **fundamental architectural flaw**: metadata is stored in TWO separate, non-synchronized systems:

1. **ChronikMetaLogStore** (local WAL-backed) - Topics, consumer offsets, groups
2. **MetadataStateMachine** (Raft-replicated) - Brokers, partition assignments, leaders

This dual-store architecture causes **split-brain metadata** where each node has divergent views of cluster state.

## The Split-Brain Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                    CURRENT (BROKEN)                          │
│                                                              │
│  Node 1                Node 2                Node 3         │
│  ┌─────────┐          ┌─────────┐          ┌─────────┐     │
│  │ Local   │          │ Local   │          │ Local   │     │
│  │ __meta  │    NO    │ __meta  │    NO    │ __meta  │     │
│  │ WAL     │<──SYNC──>│ WAL     │<──SYNC──>│ WAL     │     │
│  │         │          │         │          │         │     │
│  │ Topics: │          │ Topics: │          │ Topics: │     │
│  │  test   │          │  __meta │          │  __meta │     │
│  │  __meta │          │         │          │         │     │
│  └─────────┘          └─────────┘          └─────────┘     │
│  ↑ Diverged           ↑ Diverged           ↑ Diverged      │
│                                                              │
│  ┌─────────┐          ┌─────────┐          ┌─────────┐     │
│  │ Raft    │←────────→│ Raft    │←────────→│ Raft    │     │
│  │ State   │ Consensus │ State   │ Consensus │ State   │     │
│  │ Machine │          │ Machine │          │ Machine │     │
│  │         │          │         │          │         │     │
│  │ Brokers │          │ Brokers │          │ Brokers │     │
│  │ Part    │          │ Part    │          │ Part    │     │
│  │ Assigns │          │ Assigns │          │ Assigns │     │
│  └─────────┘          └─────────┘          └─────────┘     │
│  ↑ Consistent         ↑ Consistent         ↑ Consistent    │
│                                                              │
│  PROBLEM: Metadata is SPLIT between two systems!            │
└──────────────────────────────────────────────────────────────┘
```

## Concrete Evidence

### Test Output

```bash
$ python3 -c "from kafka.admin import KafkaAdminClient; ..."

=== Node 1 (localhost:9092) - Leader ===
Topics: ['test-kafka202', '__meta', 'test-kafka2215', 'test-topic', 'test']

=== Node 2 (localhost:9093) ===
Topics: ['__meta', 'test-kafka2215']

=== Node 3 (localhost:9094) ===
Topics: ['__meta']  # ← Only has system topic!
```

### Node 3 Logs

```
[INFO] METADATA→PARTITIONS: topic=__meta partition_count=1 assignments_count=1
[WARN] No leader found for partition __meta/0, using default broker 3
[INFO] PARTITION_REPLICAS: topic=__meta partition=0 replicas=[3]
[INFO] METADATA_PARTITION_FINAL: topic=__meta partition=0 leader_id=3 replicas=[3] isr=[3]
```

**Analysis:** Node 3 (a follower) reports itself as leader for `__meta` because it created the topic locally!

## Root Causes

### Issue #1: System Topics Created Locally (No Raft Coordination)

**Location:** `crates/chronik-common/src/metadata/metalog_store.rs:818-839`

```rust
async fn init_system_state(&self) -> Result<()> {
    // Create system topics if they don't exist
    let system_topics = vec![
        ("__consumer_offsets", 50, 3),
        ("__transaction_state", 50, 3),
        (METADATA_TOPIC, 1, 1), // "__meta"
    ];

    for (name, partitions, replication_factor) in system_topics {
        if self.get_topic(name).await?.is_none() {
            let config = TopicConfig { /* ... */ };
            self.create_topic(name, config).await?; // ← LOCAL ONLY!
        }
    }
    Ok(())
}
```

**Problem:**
- Each node checks its **local** metadata store
- If topic doesn't exist locally, creates it **locally**
- No Raft proposal, no cluster consensus
- Result: 3 nodes create 3 separate `__meta` topics

### Issue #2: Topic Metadata Not Replicated

**Location:** `crates/chronik-server/src/produce_handler.rs:2104-2114`

```rust
// Step 1: Create topic in LOCAL metadata store
let result = self.metadata_store.create_topic(topic_name, config).await?;

// Step 2: MAYBE propose to Raft (if cluster mode)
if let Some(raft) = &self.raft_cluster {
    self.initialize_raft_partitions(...).await?; // ← May fail!
}
```

**Problem:**
- Topic created locally FIRST
- Raft proposal happens SECOND (and may fail)
- If Raft proposal fails, topic exists on one node only
- Other nodes never learn about the topic

### Issue #3: Consumer Offsets Not Replicated

**Location:** `crates/chronik-common/src/metadata/metalog_store.rs:626-630`

```rust
async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
    let payload = MetadataEventPayload::OffsetCommitted {
        offset: offset.clone(),
    };
    self.append_event(payload).await  // ← Writes to LOCAL __meta WAL only!
}
```

**Problem:**
- Consumer commits offset on Node 1
- Offset stored in Node 1's `__meta` WAL
- Consumer reconnects to Node 2
- Node 2 has no knowledge of the offset
- Consumer restarts from beginning (data loss!)

### Issue #4: Partition Offsets Not Persisted

**Location:** `crates/chronik-common/src/metadata/metalog_store.rs:806-810`

```rust
async fn update_partition_offset(&self, topic: &str, partition: u32,
                                  high_watermark: i64, log_start_offset: i64) -> Result<()> {
    let mut state = self.state.write();
    let key = (topic.to_string(), partition);
    state.partition_offsets.insert(key, (hw, lso));
    Ok(())  // ← NO WAL write, NO Raft proposal!
}
```

**Problem:**
- Partition high watermarks stored in-memory only
- NOT written to `__meta` WAL
- NOT proposed to Raft
- Lost on node restart
- Diverges across nodes

### Issue #5: Bidirectional Sync is a Band-Aid

**Location:** `crates/chronik-server/src/integrated_server.rs:296-402`

```rust
// Background task: sync brokers every 10 seconds
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Direction 1: Raft → local metadata
        sync_raft_to_local().await;

        // Direction 2: local → Raft (leader only)
        if is_leader {
            sync_local_to_raft().await;
        }
    }
});
```

**Problem:**
- Only syncs **brokers** (not topics, offsets, groups)
- 10-second delay for consistency (clients see stale data)
- Doesn't address root cause (dual metadata stores)
- Complex retry logic masks underlying architectural flaw

## Data Flow Analysis

### Startup Sequence (Current)

```
IntegratedKafkaServer::new() flow:

1. Create WAL config for metadata
   └─ Path: {data_dir}/wal_metadata

2. Create WalMetadataAdapter
   └─ Bridges ChronikMetaLogStore to WAL

3. Create ChronikMetaLogStore (LOCAL)
   ├─ Passes WalMetadataAdapter
   ├─ Snapshots: {data_dir}/metalog_snapshots
   └─ Calls recover() to replay LOCAL WAL

4. Ensure __meta topic exists
   ├─ Check if __meta exists in LOCAL store
   ├─ If not, create with RF from cluster config
   └─ ❌ CREATED VIA LOCAL STORE, NO RAFT!

5. Register broker in LOCAL metadata store
   ├─ Standalone: direct registration
   └─ Cluster: retry loop (30 attempts)

6. (Cluster only) Propose broker to Raft
   ├─ raft.propose(RegisterBroker)
   ├─ Retry loop: 15 attempts * 2s
   └─ May fail if not leader yet

7. Start bidirectional broker sync (band-aid)
8. Restore high watermarks from segments
9. Create storage, WAL, handlers
10. WAL recovery & replay

PROBLEM: __meta created locally (step 4) BEFORE Raft coordination!
```

### Metadata Query Flow (Current)

```
Client → Node 3 → Metadata API:

1. Client requests metadata for __meta topic
2. Node 3 queries LOCAL ChronikMetaLogStore
3. Local store returns: __meta exists with 1 partition
4. Node 3 checks partition assignments in LOCAL store
5. Local assignment: __meta/0 → Node 3 (leader)
6. Response: { topic: "__meta", partition: 0, leader: 3, replicas: [3] }

Meanwhile, Raft state machine has:
  - __meta/0 → Node 1 (leader), replicas: [1, 2, 3]

RESULT: Client sees Node 3 as leader, but Raft says Node 1!
```

## Component Relationships

### ChronikMetaLogStore

**Purpose:** Event-sourced metadata store with WAL persistence

**Design:**
```rust
pub struct ChronikMetaLogStore {
    wal: Arc<dyn MetaLogWalInterface>,      // WalMetadataAdapter
    state: Arc<RwLock<MetadataState>>,      // In-memory state
    event_log: Arc<RwLock<EventLog>>,       // Recent events
    snapshot_path: PathBuf,                  // Snapshot storage
}

pub struct MetadataState {
    topics: HashMap<String, TopicMetadata>,
    brokers: HashMap<i32, BrokerMetadata>,
    partition_assignments: HashMap<(String, u32), PartitionAssignment>,
    consumer_groups: HashMap<String, ConsumerGroupMetadata>,
    consumer_offsets: HashMap<(String, String, u32), ConsumerOffset>,
    partition_offsets: HashMap<(String, u32), (i64, i64)>,
}
```

**Key Methods:**
- `create_topic()` - Appends TopicCreated event to LOCAL __meta WAL
- `commit_offset()` - Appends OffsetCommitted event to LOCAL __meta WAL
- `update_partition_offset()` - In-memory only (NOT persisted!)

**Problem:** Designed for single-node use, NO replication mechanism!

### WalMetadataAdapter

**Purpose:** Bridge between ChronikMetaLogStore and WAL system

**Location:** `crates/chronik-storage/src/metadata_wal_adapter.rs`

```rust
pub struct WalMetadataAdapter {
    wal: Arc<GroupCommitWal>,  // Actual WAL
}

impl MetaLogWalInterface for WalMetadataAdapter {
    async fn append_metadata_event(&self, event: &MetadataEvent) -> Result<u64> {
        // Serialize event to JSON
        let data = serde_json::to_vec(event)?;

        // Create WalRecord V2
        let record = WalRecord::new_v2("__meta".to_string(), 0, data);

        // Append to __meta topic, partition 0
        self.wal.append_batch("__meta", 0, vec![record]).await
    }

    async fn read_metadata_events(&self, from_offset: u64) -> Result<Vec<MetadataEvent>> {
        // Read from __meta WAL
        let records = self.wal.read_from("__meta", 0, from_offset).await?;

        // Deserialize JSON back to events
        records.into_iter().map(|r| serde_json::from_slice(&r.data)).collect()
    }
}
```

**Problem:** Writes to LOCAL __meta WAL only, no cross-node replication!

### MetadataStateMachine (Raft)

**Purpose:** Raft-replicated metadata for partition coordination

**Location:** `crates/chronik-server/src/raft_metadata.rs`

```rust
pub struct MetadataStateMachine {
    nodes: HashMap<u64, String>,                        // Raft nodes
    brokers: HashMap<i32, BrokerInfo>,                  // Kafka brokers
    partition_assignments: HashMap<PartitionKey, Vec<u64>>,  // Which nodes
    partition_leaders: HashMap<PartitionKey, u64>,      // Leader per partition
    isr_sets: HashMap<PartitionKey, Vec<u64>>,         // In-sync replicas
}

pub enum MetadataCommand {
    RegisterBroker { broker_id: i32, host: String, port: i32, rack: Option<String> },
    AssignPartition { topic: String, partition: u32, replicas: Vec<u64> },
    SetPartitionLeader { topic: String, partition: u32, leader: u64 },
    UpdateISR { topic: String, partition: u32, isr: Vec<u64> },
}
```

**What it manages:**
- ✅ Brokers
- ✅ Partition assignments
- ✅ Partition leaders
- ✅ ISR sets

**What it DOESN'T manage:**
- ❌ Topics (names, configs)
- ❌ Consumer groups
- ❌ Consumer offsets
- ❌ Segment metadata
- ❌ Partition high watermarks

**Problem:** Incomplete! Should be single source of truth for ALL metadata!

## Why This is Critical

### Data Loss Scenarios

**Scenario 1: Consumer Offset Loss**
```
1. Consumer commits offset 1000 to Node 1
2. Offset stored in Node 1's __meta WAL
3. Node 1 crashes
4. Consumer reconnects to Node 2
5. Node 2 has no record of offset 1000
6. Consumer restarts from offset 0
7. Result: 1000 messages processed twice (duplicate processing)
```

**Scenario 2: Topic Metadata Divergence**
```
1. Producer creates topic "orders" on Node 1
2. Topic stored in Node 1's __meta WAL
3. Raft proposal fails (network partition)
4. Node 2 and Node 3 have no knowledge of "orders"
5. Consumer connects to Node 2, asks for "orders"
6. Node 2 returns UNKNOWN_TOPIC_OR_PARTITION
7. Result: Producer writes, consumer can't read (data invisible)
```

**Scenario 3: Partition Leadership Confusion**
```
1. Node 1's __meta says: topic=test partition=0 leader=1
2. Node 3's __meta says: topic=test partition=0 leader=3
3. Raft state machine says: topic=test partition=0 leader=2
4. Client connects to Node 3, gets leader=3
5. Client sends produce to Node 3
6. Node 3 accepts write (thinks it's leader)
7. Meanwhile, Node 2 is ACTUAL leader, also accepting writes
8. Result: Split-brain writes, data corruption
```

### Production Impact

- **Data loss:** Consumer offsets not replicated
- **Duplicate processing:** Consumers restart from beginning
- **Invisible topics:** Topics created on one node only
- **Split-brain writes:** Multiple leaders for same partition
- **Metadata inconsistency:** Each node has different view
- **Client confusion:** Different responses from different nodes

## Current Workarounds (Insufficient)

### 1. Bidirectional Broker Sync

**Code:** `integrated_server.rs:296-402`

**What it does:**
- Every 10 seconds, sync broker metadata
- Direction 1: Raft → local metadata store
- Direction 2: local metadata → Raft (leader only)

**Why insufficient:**
- Only syncs **brokers** (not topics, offsets, groups)
- 10-second delay (stale metadata)
- Doesn't prevent local topic creation
- Complex retry logic
- Masks root cause

### 2. Raft Partition Initialization

**Code:** `produce_handler.rs:2173-2193`

**What it does:**
- After creating topic locally, propose partitions to Raft
- Only proposes if node is Raft leader
- Retries on failure

**Why insufficient:**
- Topic created locally FIRST (already inconsistent)
- Raft proposal may fail (network, not leader, etc.)
- No rollback if Raft proposal fails
- Other nodes may create same topic independently

### 3. Manual Recovery on Restart

**Code:** `metalog_store.rs:280-309`

**What it does:**
- On restart, replay __meta WAL to rebuild state
- Load latest snapshot if available

**Why insufficient:**
- Only recovers **local** state
- No cross-node synchronization
- Divergence persists across restarts

## Architectural Design Flaws

### Flaw #1: No Separation of Concerns

ChronikMetaLogStore manages:
- Topics (cluster-wide concern)
- Brokers (cluster-wide concern)
- Consumer offsets (cluster-wide concern)
- Partition offsets (local concern)

**Should be:**
- **Raft State Machine:** Topics, brokers, offsets (replicated)
- **Local Storage:** Segment metadata, caches (local)

### Flaw #2: Dual Source of Truth

Two metadata stores:
- ChronikMetaLogStore (local)
- MetadataStateMachine (Raft)

**Should be:**
- Single source: Raft State Machine (cluster mode)
- ChronikMetaLogStore only for standalone mode

### Flaw #3: Write-Before-Consensus

Pattern throughout code:
```rust
// 1. Write to local store
metadata_store.create_topic(...).await?;

// 2. Maybe propose to Raft
if cluster_mode {
    raft.propose(...).await?; // ← May fail!
}
```

**Should be:**
```rust
// 1. Propose to Raft FIRST
raft.propose(CreateTopic { ... }).await?;

// 2. Wait for Raft commit

// 3. Apply to local state (via Raft state machine)
```

### Flaw #4: System Topics Not Special-Cased

System topics treated like regular topics:
- Created via local metadata store
- No Raft coordination
- Each node creates independently

**Should be:**
- System topics created via Raft (leader proposes)
- All nodes apply from Raft log
- Guaranteed consistency

## Summary of Issues

| Issue | Location | Impact | Severity |
|-------|----------|--------|----------|
| System topics created locally | `metalog_store.rs:818-839` | Split-brain __meta topic | CRITICAL |
| Topic metadata not replicated | `produce_handler.rs:2104` | Topics invisible on some nodes | CRITICAL |
| Consumer offsets not replicated | `metalog_store.rs:626-630` | Data loss on failover | CRITICAL |
| Partition offsets not persisted | `metalog_store.rs:806-810` | Lost on restart | HIGH |
| Bidirectional sync incomplete | `integrated_server.rs:296-402` | Only syncs brokers | MEDIUM |
| Dual metadata stores | Architecture | Complexity, bugs | HIGH |
| Write-before-consensus | Multiple locations | Consistency violations | CRITICAL |

## Next Steps

See `docs/METADATA_SPLIT_BRAIN_FIX_PLAN.md` for comprehensive fix strategy.

**Estimated effort:** Medium-Large refactoring (2-3 days)

**Key changes:**
1. Expand Raft state machine to include ALL metadata
2. Implement RaftMetadataStore (wraps Raft)
3. Switch cluster mode to use RaftMetadataStore
4. Keep ChronikMetaLogStore for standalone mode only
5. System topics via Raft proposal (leader only)
6. All metadata writes through Raft (propose → commit → apply)
7. Comprehensive testing

**Migration path:**
- Backward compatible: Standalone mode unchanged
- Cluster mode: Fresh bootstrap (existing clusters need reset)
- Future: Snapshot import for migration
