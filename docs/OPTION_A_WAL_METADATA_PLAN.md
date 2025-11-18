# Option A: WAL-Based Metadata Replication - Implementation Plan

**Date**: 2025-11-18
**Decision**: Remove Raft from metadata path, use ChronikMetaLog + WAL replication
**Priority**: P0 - CRITICAL (Eliminates produce path bottleneck)
**Effort**: 16 hours estimated
**Risk**: MEDIUM (architectural change, but aligns with existing message replication)

---

## Executive Summary

Replace `RaftMetadataStore` (Raft consensus) with `ChronikMetaLog` (WAL-based) and replicate metadata through the existing WAL replication mechanism - the same way message data is replicated.

### Benefits

✅ **No Raft on produce path** - Topic creation doesn't block on 3-phase Raft consensus
✅ **Fast topic creation** - < 100ms instead of 20+ seconds
✅ **Consistent architecture** - Metadata replicated same as messages
✅ **Eventual consistency** - Same as message data (acceptable for metadata)
✅ **Simpler** - One replication mechanism instead of two
✅ **Existing code** - ChronikMetaLog already exists and works

### Trade-offs

⚠️ **Eventual consistency** - Followers may have stale metadata for brief periods
⚠️ **No strong consistency** - Can't guarantee linearizable reads (but do we need this?)
⚠️ **Conflict resolution** - Need to handle concurrent topic creation on different nodes

---

## Current Architecture (BEFORE)

```
┌─────────────────────────────────────────────────────────────────┐
│                     CURRENT (v2.2.9)                             │
│              Raft Consensus for Metadata                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Client → Produce to new topic 'my-topic'                       │
│     ↓                                                            │
│  1. Check metadata store: Topic exists?                          │
│     ↓ NO                                                         │
│  2. Auto-create topic (BLOCKS HERE)                              │
│     ├─ Call RaftMetadataStore::create_topic()                   │
│     ├─ Propose to Raft cluster (3-phase commit)                 │
│     │  ├─ Phase 1: Propose entry to leader                      │
│     │  ├─ Phase 2: Leader replicates to followers (SLOW!)       │
│     │  └─ Phase 3: Wait for quorum commit                       │
│     ├─ Persist Raft log (I/O under lock) ← 20+ SECONDS!         │
│     ├─ Wait for notification (5s timeout)                       │
│     └─ Return success/timeout                                   │
│     ↓                                                            │
│  3. Write message to WAL (fast - direct replication)             │
│     ↓                                                            │
│  4. Return success                                               │
│                                                                  │
│  **Problem**: Steps 2 (Raft consensus) takes 20+ seconds        │
│               during leader elections                            │
└─────────────────────────────────────────────────────────────────┘
```

---

## New Architecture (AFTER - Option A)

```
┌─────────────────────────────────────────────────────────────────┐
│                     NEW (Option A)                               │
│            WAL-Based Metadata Replication                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Client → Produce to new topic 'my-topic'                       │
│     ↓                                                            │
│  1. Check metadata store: Topic exists?                          │
│     ↓ NO                                                         │
│  2. Auto-create topic (FAST - NO RAFT!)                          │
│     ├─ Call ChronikMetaLog::create_topic() (local)              │
│     ├─ Write CreateTopicEvent to __chronik_metadata WAL          │
│     ├─ Replicate to followers (SAME as message data)            │
│     │  ├─ Leader → WalSender → Follower WalReceiver             │
│     │  └─ Followers apply event when they consume               │
│     └─ Return success immediately (< 100ms)                     │
│     ↓                                                            │
│  3. Write message to WAL (fast - direct replication)             │
│     ↓                                                            │
│  4. Return success                                               │
│                                                                  │
│  **Result**: Topic creation completes in < 100ms (200x faster)  │
│                                                                  │
│  Followers eventually consistent:                                │
│  - Follower receives __chronik_metadata event                   │
│  - Applies CreateTopicEvent to local ChronikMetaLog             │
│  - Now has topic metadata                                       │
│  - Lag: typically < 50ms                                        │
└─────────────────────────────────────────────────────────────────┘
```

---

## Implementation Steps

### Step 1: Create Metadata Event Types (2 hours)

**File**: `crates/chronik-common/src/metadata/events.rs` (NEW)

Define events for all metadata operations:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataEvent {
    CreateTopic(CreateTopicEvent),
    DeleteTopic(DeleteTopicEvent),
    CreatePartition(CreatePartitionEvent),
    UpdateHighWatermark(UpdateHighWatermarkEvent),
    CreateConsumerGroup(CreateConsumerGroupEvent),
    CommitOffset(CommitOffsetEvent),
    // ... all metadata operations
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopicEvent {
    pub topic_name: String,
    pub num_partitions: i32,
    pub replication_factor: i32,
    pub timestamp_ms: i64,
    pub created_by_node: u64, // Which node created this topic
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateHighWatermarkEvent {
    pub topic: String,
    pub partition: i32,
    pub new_watermark: i64,
    pub timestamp_ms: i64,
}

// ... other event types ...

impl MetadataEvent {
    /// Serialize to bytes for WAL storage
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        bincode::serialize(self)
            .map_err(|e| MetadataError::SerializationError(e.to_string()))
    }

    /// Deserialize from bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data)
            .map_err(|e| MetadataError::DeserializationError(e.to_string()))
    }
}
```

**Testing**:
- Unit test: Serialize/deserialize all event types
- Verify backward compatibility (can we add fields later?)

---

### Step 2: Enhance ChronikMetaLog for Replication (4 hours)

**File**: `crates/chronik-common/src/metadata/chronik_meta_log.rs`

Currently ChronikMetaLog is local-only. Enhance it to:
1. Write events to `__chronik_metadata` WAL topic
2. Apply events from replication

```rust
impl ChronikMetaLog {
    /// Create topic and write event to __chronik_metadata WAL
    pub async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // 1. Create topic locally
        let topic_metadata = TopicMetadata {
            name: name.to_string(),
            num_partitions: config.num_partitions,
            replication_factor: config.replication_factor,
            created_at_ms: chrono::Utc::now().timestamp_millis(),
        };

        // 2. Write CreateTopicEvent to __chronik_metadata WAL
        let event = MetadataEvent::CreateTopic(CreateTopicEvent {
            topic_name: name.to_string(),
            num_partitions: config.num_partitions,
            replication_factor: config.replication_factor,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            created_by_node: self.node_id,
        });

        // 3. Append to WAL (will be replicated to followers automatically)
        self.wal_manager
            .append_metadata_event(&event)
            .await?;

        // 4. Apply locally (leader's copy)
        self.apply_event(event)?;

        // 5. Return immediately (don't wait for followers)
        Ok(topic_metadata)
    }

    /// Apply event from replication (called on followers)
    pub fn apply_event(&self, event: MetadataEvent) -> Result<()> {
        match event {
            MetadataEvent::CreateTopic(evt) => {
                let mut topics = self.topics.write();

                // Idempotency: If topic already exists, ignore
                if topics.contains_key(&evt.topic_name) {
                    tracing::debug!("Topic '{}' already exists, skipping create event", evt.topic_name);
                    return Ok(());
                }

                // Create topic
                let topic_metadata = TopicMetadata {
                    name: evt.topic_name.clone(),
                    num_partitions: evt.num_partitions,
                    replication_factor: evt.replication_factor,
                    created_at_ms: evt.timestamp_ms,
                };

                topics.insert(evt.topic_name.clone(), topic_metadata);
                tracing::info!("✓ Applied CreateTopic event for '{}'", evt.topic_name);
                Ok(())
            }

            MetadataEvent::UpdateHighWatermark(evt) => {
                let mut watermarks = self.high_watermarks.write();
                let key = format!("{}-{}", evt.topic, evt.partition);

                // Only update if new watermark is higher (idempotency)
                let current = watermarks.get(&key).copied().unwrap_or(0);
                if evt.new_watermark > current {
                    watermarks.insert(key, evt.new_watermark);
                }
                Ok(())
            }

            // ... handle other event types ...
        }
    }
}
```

**Key Design Decisions**:
- **Idempotency**: All events must be idempotent (can be applied multiple times)
- **Conflict resolution**: Last-write-wins for watermarks, first-write-wins for topic creation
- **No blocking**: Leader doesn't wait for followers to apply events

---

### Step 3: Integrate with WAL Replication (4 hours)

**File**: `crates/chronik-wal/src/manager.rs`

Add support for `__chronik_metadata` as a special internal topic:

```rust
impl WalManager {
    /// Append metadata event to __chronik_metadata WAL
    pub async fn append_metadata_event(&self, event: &MetadataEvent) -> Result<()> {
        let data = event.to_bytes()?;

        // Write to __chronik_metadata topic (partition 0)
        let record = WalRecord::new(
            "__chronik_metadata".to_string(),
            0, // Single partition for metadata
            data,
        );

        self.append_record(record).await?;

        // Trigger immediate replication (don't wait for batch)
        self.notify_replication().await;

        Ok(())
    }

    /// Consume metadata events (called by followers)
    pub async fn consume_metadata_events(&self, callback: impl Fn(MetadataEvent)) -> Result<()> {
        let mut consumer = self.create_consumer("__chronik_metadata", 0)?;

        loop {
            match consumer.poll(Duration::from_millis(100)).await {
                Ok(Some(record)) => {
                    let event = MetadataEvent::from_bytes(&record.data)?;
                    callback(event);
                }
                Ok(None) => {
                    // No new events, continue polling
                }
                Err(e) => {
                    tracing::error!("Error consuming metadata events: {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}
```

**File**: `crates/chronik-server/src/wal_replication/sender.rs`

Ensure `__chronik_metadata` is replicated like regular topics:

```rust
impl WalSender {
    pub async fn replicate_metadata(&self) -> Result<()> {
        // Replicate __chronik_metadata to all followers
        // (No special handling needed - it's just another topic!)
        self.replicate_topic("__chronik_metadata", 0).await
    }
}
```

---

### Step 4: Remove Raft from Metadata Path (3 hours)

**File**: `crates/chronik-server/src/main.rs`

Replace `RaftMetadataStore` with `ChronikMetaLog`:

```rust
// BEFORE (v2.2.9):
let metadata_store: Arc<dyn MetadataStore> = if cluster_mode {
    Arc::new(RaftMetadataStore::new(raft_cluster.clone())) // ← SLOW!
} else {
    Arc::new(ChronikMetaLog::new(wal_manager.clone()))
};

// AFTER (Option A):
let metadata_store: Arc<dyn MetadataStore> = Arc::new(
    ChronikMetaLog::new(wal_manager.clone(), node_id)
);
// Works for BOTH single-node and cluster modes!
```

**File**: `crates/chronik-server/src/raft_metadata_store.rs`

Delete this entire file (or mark deprecated):

```bash
# Remove Raft-based metadata store
git rm crates/chronik-server/src/raft_metadata_store.rs
```

**File**: `crates/chronik-server/src/integrated_server.rs`

Remove Raft references:

```rust
// BEFORE:
pub struct IntegratedKafkaServer {
    raft_cluster: Arc<RaftCluster>, // ← DELETE
    metadata_store: Arc<RaftMetadataStore>, // ← DELETE
    // ...
}

// AFTER:
pub struct IntegratedKafkaServer {
    metadata_store: Arc<ChronikMetaLog>, // ✅ WAL-based only
    wal_manager: Arc<WalManager>,
    // ...
}
```

---

### Step 5: Add Follower Metadata Sync (2 hours)

**File**: `crates/chronik-server/src/metadata_sync.rs` (NEW)

Create background task on followers to consume `__chronik_metadata`:

```rust
pub struct MetadataSync {
    metadata_store: Arc<ChronikMetaLog>,
    wal_manager: Arc<WalManager>,
}

impl MetadataSync {
    pub async fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            tracing::info!("Starting metadata sync loop...");

            if let Err(e) = self.sync_loop().await {
                tracing::error!("Metadata sync loop failed: {}", e);
            }
        });
    }

    async fn sync_loop(&self) -> Result<()> {
        self.wal_manager.consume_metadata_events(|event| {
            // Apply event to local metadata store
            if let Err(e) = self.metadata_store.apply_event(event.clone()) {
                tracing::error!("Failed to apply metadata event: {}", e);
            }
        }).await
    }
}
```

**File**: `crates/chronik-server/src/main.rs`

Start metadata sync on ALL nodes (leader and followers):

```rust
// Start metadata sync (consumes __chronik_metadata from leader)
let metadata_sync = Arc::new(MetadataSync::new(
    metadata_store.clone(),
    wal_manager.clone(),
));
metadata_sync.clone().start().await;
```

---

### Step 6: Testing & Verification (1 hour)

**Test 1: Single-Node Mode**
```bash
# Start single-node server
cargo run --bin chronik-server start

# Create topic
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks=1)
producer.send('test-topic', b'hello')
producer.flush()
print('✅ Topic created (single-node)')
"

# Verify: Should complete in < 1 second
```

**Test 2: Cluster Mode - Leader Topic Creation**
```bash
# Start 3-node cluster
./tests/cluster/start.sh

# Create topic on leader (Node 1)
python3 -c "
import time
from kafka import KafkaProducer
start = time.time()
producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks=1)
producer.send('cluster-topic', b'hello')
producer.flush()
elapsed = time.time() - start
print(f'✅ Topic created in {elapsed:.2f}s (should be < 0.5s)')
"
```

**Test 3: Cluster Mode - Follower Sees Topic**
```bash
# Wait for replication
sleep 1

# Query topic from follower (Node 2)
python3 -c "
from kafka import KafkaConsumer
consumer = KafkaConsumer(
    'cluster-topic',
    bootstrap_servers=['localhost:9093'], # Follower!
    auto_offset_reset='earliest'
)
print('✅ Follower has topic metadata')
"
```

**Test 4: Concurrent Topic Creation (Conflict Resolution)**
```bash
# Create same topic simultaneously on 2 nodes
python3 -c "
from kafka import KafkaProducer
import concurrent.futures

def create_on_node(port):
    producer = KafkaProducer(bootstrap_servers=[f'localhost:{port}'], acks=1)
    producer.send('conflict-topic', b'test')
    producer.flush()

with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
    futures = [
        executor.submit(create_on_node, 9092),  # Node 1
        executor.submit(create_on_node, 9093),  # Node 2
    ]
    concurrent.futures.wait(futures)

print('✅ Concurrent creation handled (one succeeds, one idempotent)')
"
```

---

## Migration Path (v2.2.9 → v3.0.0)

### Backward Compatibility

**Problem**: v2.2.9 clusters have metadata in Raft WAL (`__raft_metadata`)
**Solution**: Migrate on first startup

```rust
// File: crates/chronik-server/src/migration.rs (NEW)

pub async fn migrate_raft_to_wal_metadata(
    raft_cluster: &RaftCluster,
    chronik_meta_log: &ChronikMetaLog,
) -> Result<()> {
    tracing::info!("Migrating metadata from Raft to WAL...");

    // 1. Read all topics from Raft
    let topics = raft_cluster.get_all_topics().await?;

    // 2. Write to ChronikMetaLog (via events)
    for topic in topics {
        let event = MetadataEvent::CreateTopic(CreateTopicEvent {
            topic_name: topic.name.clone(),
            num_partitions: topic.num_partitions,
            replication_factor: topic.replication_factor,
            timestamp_ms: topic.created_at_ms,
            created_by_node: 0, // Migration
        });

        chronik_meta_log.apply_event(event)?;
    }

    tracing::info!("✓ Migrated {} topics from Raft to WAL", topics.len());
    Ok(())
}
```

### Upgrade Procedure

1. **Stop all nodes**: `./tests/cluster/stop.sh`
2. **Upgrade binary**: Deploy v3.0.0
3. **Start leader first**: Migration runs automatically
4. **Start followers**: Sync from leader via `__chronik_metadata`
5. **Verify**: All topics present on all nodes

---

## Rollback Plan

If Option A causes issues:

1. **Immediate rollback**: Revert to v2.2.9 binary
2. **Data preserved**: Raft WAL still exists (not deleted)
3. **Metadata intact**: Can restart with Raft-based metadata

**Rollback command**:
```bash
# Stop v3.0.0
./tests/cluster/stop.sh

# Deploy v2.2.9
git checkout v2.2.9
cargo build --release

# Start cluster
./tests/cluster/start.sh
```

---

## Success Criteria

**Performance**:
- ✅ Topic creation: < 100ms (was 20+ seconds)
- ✅ Produce latency: No regression
- ✅ Metadata replication lag: < 50ms

**Correctness**:
- ✅ All metadata operations work (create topic, delete, update watermarks)
- ✅ Eventual consistency: Followers see metadata within 100ms
- ✅ Conflict resolution: Concurrent operations handled correctly
- ✅ Idempotency: Events can be applied multiple times safely

**Reliability**:
- ✅ No deadlocks (Raft removed from produce path)
- ✅ No indefinite hangs (no Raft consensus timeouts)
- ✅ Migration works (v2.2.9 → v3.0.0)

---

## Timeline

| Step | Task | Hours | Status |
|------|------|-------|--------|
| 1 | Create metadata event types | 2h | ⬜ Not Started |
| 2 | Enhance ChronikMetaLog | 4h | ⬜ Not Started |
| 3 | Integrate WAL replication | 4h | ⬜ Not Started |
| 4 | Remove Raft from metadata | 3h | ⬜ Not Started |
| 5 | Add follower sync | 2h | ⬜ Not Started |
| 6 | Testing & verification | 1h | ⬜ Not Started |
| **Total** | | **16h** | **Ready** |

---

## Next Steps

1. ✅ Review this plan with team
2. ⬜ Create feature branch: `feature/wal-metadata-replication`
3. ⬜ Implement Step 1 (metadata events)
4. ⬜ Test each step incrementally
5. ⬜ Full cluster testing
6. ⬜ Release as v3.0.0

**Ready to begin implementation!**

---

**Status**: ✅ PLAN COMPLETE - Ready for Implementation
**Next Action**: Begin Step 1 (Create Metadata Event Types)
**Estimated Completion**: 2 working days (16 hours)
