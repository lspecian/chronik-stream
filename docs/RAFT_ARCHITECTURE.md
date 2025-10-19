# Raft Architecture in Chronik

Deep dive into Chronik's Raft-based clustering architecture, data flow, and design decisions.

## Table of Contents

- [High-Level Architecture](#high-level-architecture)
- [Multi-Raft Design](#multi-raft-design)
- [Data Flow Diagrams](#data-flow-diagrams)
- [Raft Log Storage](#raft-log-storage)
- [Metadata Replication](#metadata-replication)
- [Snapshot Mechanism](#snapshot-mechanism)
- [Multi-Partition Design](#multi-partition-design)

---

## High-Level Architecture

Chronik uses **Raft consensus** for both metadata and data replication across a cluster of nodes.

### Architecture Diagram

```
┌────────────────────────────────────────────────────────────────────┐
│                     Chronik Raft Cluster                            │
│                                                                      │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐ │
│  │  Node 1 (Leader) │  │  Node 2 (Follower)│ │  Node 3 (Follower)│ │
│  │                  │  │                  │  │                  │  │
│  │  Kafka: 9092     │  │  Kafka: 9092     │  │  Kafka: 9092     │  │
│  │  Raft: 9093      │  │  Raft: 9093      │  │  Raft: 9093      │  │
│  │                  │  │                  │  │                  │  │
│  │  ┌────────────┐  │  │  ┌────────────┐  │  │  ┌────────────┐  │  │
│  │  │ Kafka      │  │  │  │ Kafka      │  │  │  │ Kafka      │  │  │
│  │  │ Protocol   │  │  │  │ Protocol   │  │  │  │ Protocol   │  │  │
│  │  │ Handler    │  │  │  │ Handler    │  │  │  │ Handler    │  │  │
│  │  └─────┬──────┘  │  │  └─────┬──────┘  │  │  └─────┬──────┘  │  │
│  │        │         │  │        │         │  │        │         │  │
│  │  ┌─────▼──────┐  │  │  ┌─────▼──────┐  │  │  ┌─────▼──────┐  │  │
│  │  │ Raft Group │  │  │  │ Raft Group │  │  │  │ Raft Group │  │  │
│  │  │ Manager    │  │  │  │ Manager    │  │  │  │ Manager    │  │  │
│  │  └─────┬──────┘  │  │  └─────┬──────┘  │  │  └─────┬──────┘  │  │
│  │        │         │  │        │         │  │        │         │  │
│  │  ┌─────▼──────────────────────────────────────────▼──────┐  │  │
│  │  │            Multi-Raft Consensus Layer                  │  │  │
│  │  │  ┌──────────┐  ┌──────────┐  ┌──────────┐             │  │  │
│  │  │  │ __meta/0 │  │ orders/0 │  │ orders/1 │  ...        │  │  │
│  │  │  │ (Metadata)│  │ (Data)   │  │ (Data)   │             │  │  │
│  │  │  └──────────┘  └──────────┘  └──────────┘             │  │  │
│  │  │         Independent Raft groups per partition          │  │  │
│  │  └──────────────────────────────────────────────────────┘  │  │
│  │        │         │  │        │         │  │        │         │  │
│  │  ┌─────▼──────┐  │  │  ┌─────▼──────┐  │  │  ┌─────▼──────┐  │  │
│  │  │ WAL        │  │  │  │ WAL        │  │  │  │ WAL        │  │  │
│  │  │ Storage    │  │  │  │ Storage    │  │  │  │ Storage    │  │  │
│  │  └────────────┘  │  │  └────────────┘  │  │  └────────────┘  │  │
│  └──────────────────┘  └──────────────────┘  └──────────────────┘  │
│                                                                      │
│  Replication: Leader → Followers (via Raft AppendEntries)          │
│  Quorum: 2/3 nodes required for commit                              │
└────────────────────────────────────────────────────────────────────┘
```

### Key Components

1. **Kafka Protocol Handler**: Handles incoming Kafka API requests (Produce, Fetch, etc.)
2. **Raft Group Manager**: Manages multiple independent Raft groups (one per partition)
3. **Partition Replica**: Single Raft group instance for one partition
4. **WAL Storage**: Persistent Raft log storage (backed by Chronik's WAL)
5. **State Machine**: Applies committed Raft entries to partition state

---

## Multi-Raft Design

Chronik uses **Multi-Raft**, where each partition is an independent Raft group with its own:
- Leader election
- Log replication
- Commit index
- Applied index
- Snapshots

This design provides:
- **Scalability**: Partitions can be distributed across nodes
- **Fault isolation**: One partition's issues don't affect others
- **Parallel processing**: Multiple Raft groups operate concurrently

### Special Metadata Partition

The `__meta` partition (topic `__meta`, partition `0`) is a special Raft group that stores:
- Topic metadata (name, config, partitions)
- Partition assignments (which nodes host which partitions)
- Consumer group state (members, offsets)
- Cluster configuration

All cluster-wide metadata operations go through the `__meta` Raft group, ensuring strong consistency.

---

## Data Flow Diagrams

### Produce Flow (Write Path)

```
┌──────────┐
│ Producer │
└────┬─────┘
     │
     │ 1. ProduceRequest (via Kafka protocol)
     ▼
┌──────────────────────┐
│  Any Node            │
│  (Leader or Follower)│
└────┬─────────────────┘
     │
     │ 2. If follower: forward to leader
     │    If leader: continue
     ▼
┌──────────────────────┐
│  Partition Leader    │
│  (orders-0)          │
└────┬─────────────────┘
     │
     │ 3. Propose to Raft
     │    (create Raft log entry)
     ▼
┌──────────────────────┐
│  Raft Leader         │
│  (orders-0)          │
└────┬─────────────────┘
     │
     │ 4. AppendEntries RPC to followers
     │
     ├─────────────┬─────────────┐
     ▼             ▼             ▼
┌─────────┐   ┌─────────┐   ┌─────────┐
│ Node 1  │   │ Node 2  │   │ Node 3  │
│ WAL     │   │ WAL     │   │ WAL     │
└────┬────┘   └────┬────┘   └────┬────┘
     │             │             │
     │ 5. Persist to local WAL (fsync)
     │             │             │
     ├─────────────┴─────────────┤
     │                           │
     │ 6. Respond to leader      │
     ▼                           │
┌──────────────────────┐         │
│  Raft Leader         │◄────────┘
│  (orders-0)          │
└────┬─────────────────┘
     │
     │ 7. Quorum achieved (2/3 nodes)
     │    Commit entry, update commit_index
     ▼
┌──────────────────────┐
│  State Machine       │
│  (apply to partition)│
└────┬─────────────────┘
     │
     │ 8. ProduceResponse to client
     ▼
┌──────────┐
│ Producer │
└──────────┘
```

**Steps:**
1. Producer sends ProduceRequest to any node
2. If request goes to follower, forward to leader
3. Leader proposes entry to Raft (creates log entry)
4. Leader sends AppendEntries RPC to followers
5. All nodes persist entry to WAL (fsync)
6. Followers respond to leader
7. Once quorum (2/3) acknowledges, leader commits entry
8. State machine applies entry, ProduceResponse sent to client

**Latency breakdown (typical):**
- Network RTT (client → server): 1-5ms
- Raft quorum replication: 5-20ms
- WAL fsync: 1-5ms
- **Total p99**: 20-50ms

---

### Fetch Flow (Read Path)

```
┌──────────┐
│ Consumer │
└────┬─────┘
     │
     │ 1. FetchRequest (offset=1000)
     ▼
┌──────────────────────┐
│  Any Node            │
│  (Leader or Follower)│
└────┬─────────────────┘
     │
     │ 2. Check local applied_index
     │    (has data been replicated here?)
     ▼
┌──────────────────────┐
│  Local WAL Buffer    │
│  (Tier 1)            │
└────┬─────────────────┘
     │
     │ 3. If data in WAL: serve from WAL (< 1ms)
     │    Else: try Tier 2 (raw segments)
     ▼
┌──────────────────────┐
│  S3 Raw Segments     │
│  (Tier 2)            │
└────┬─────────────────┘
     │
     │ 4. If not in S3: try Tier 3 (Tantivy)
     │    (For old data, after local WAL deleted)
     ▼
┌──────────────────────┐
│  Tantivy Indexes     │
│  (Tier 3, S3)        │
└────┬─────────────────┘
     │
     │ 5. FetchResponse with records
     ▼
┌──────────┐
│ Consumer │
└──────────┘
```

**Important:** Fetch can be served from **any node** (leader or follower), as long as the node has replicated the data up to the requested offset.

**Latency breakdown:**
- Tier 1 (WAL): < 1ms
- Tier 2 (S3 raw): 50-200ms
- Tier 3 (Tantivy): 100-500ms

---

### Metadata Operation Flow

```
┌──────────────────┐
│ Kafka Client     │
│ (CreateTopics)   │
└────┬─────────────┘
     │
     │ 1. CreateTopicsRequest
     ▼
┌──────────────────┐
│  Any Node        │
└────┬─────────────┘
     │
     │ 2. Forward to __meta leader
     ▼
┌──────────────────┐
│  __meta Leader   │
│  (Node 1)        │
└────┬─────────────┘
     │
     │ 3. Propose metadata change to Raft
     │    (e.g., CreateTopic operation)
     ▼
┌──────────────────┐
│  Raft (__meta/0) │
└────┬─────────────┘
     │
     │ 4. Replicate to followers via AppendEntries
     │
     ├─────────────┬─────────────┐
     ▼             ▼             ▼
┌─────────┐   ┌─────────┐   ┌─────────┐
│ Node 1  │   │ Node 2  │   │ Node 3  │
│ __meta  │   │ __meta  │   │ __meta  │
│ WAL     │   │ WAL     │   │ WAL     │
└────┬────┘   └────┬────┘   └────┬────┘
     │             │             │
     │ 5. Quorum commits
     │             │             │
     ├─────────────┴─────────────┤
     │                           │
     ▼                           │
┌──────────────────┐             │
│  Metadata State  │◄────────────┘
│  Machine (apply) │
└────┬─────────────┘
     │
     │ 6. Topic created in all nodes' metadata
     │    CreateTopicsResponse to client
     ▼
┌──────────────────┐
│ Kafka Client     │
└──────────────────┘
```

**Guarantees:**
- **Strong consistency**: All nodes see same metadata state
- **Ordered operations**: Metadata changes applied in same order on all nodes
- **Durability**: Metadata persisted to WAL, survives node failures

---

## Raft Log Storage

Chronik reuses its existing **GroupCommitWal** as the Raft log storage backend.

### WAL Structure for Raft

```
/data/chronik/wal/
  ├── __meta/                    # Metadata partition
  │   └── 0/                     # Partition 0
  │       ├── wal_0_0.log        # Raft log segment 0
  │       ├── wal_0_1.log        # Raft log segment 1
  │       └── snapshot_12345.bin # Raft snapshot
  ├── orders/                    # Data topic
  │   ├── 0/                     # Partition 0
  │   │   ├── wal_0_0.log
  │   │   ├── wal_0_1.log
  │   │   └── snapshot_5000.bin
  │   └── 1/                     # Partition 1
  │       ├── wal_0_0.log
  │       └── wal_0_1.log
  └── users/
      └── 0/
          └── wal_0_0.log
```

### Raft Entry Format

Each Raft log entry contains:

```rust
pub struct RaftEntry {
    /// Raft log index (monotonic)
    pub index: u64,

    /// Raft term
    pub term: u64,

    /// Entry type (Normal, ConfigChange, Snapshot)
    pub entry_type: EntryType,

    /// Entry data (serialized CanonicalRecord or metadata operation)
    pub data: Vec<u8>,

    /// CRC32 checksum
    pub checksum: u32,
}
```

**Differences from regular WAL records:**
- Raft entries include `term` for consensus
- Raft entries are ordered by `index` (global log position)
- Regular WAL records are ordered by offset (partition-specific)

### Log Compaction

Raft logs grow unbounded without compaction. Chronik uses **snapshot-based compaction**:

1. When log reaches `snapshot_threshold` entries, create snapshot
2. Snapshot contains full partition state at index N
3. Truncate log entries ≤ N (they're now in snapshot)
4. Recovery: Apply snapshot + replay entries > N

**Example:**
```
Before compaction:
  Entries: 0, 1, 2, ..., 10000, 10001, 10002
  Log size: 100MB

After snapshot at index 10000:
  Snapshot: 0-10000 (compressed, 30MB)
  Entries: 10001, 10002, ...
  Log size: 30MB + 0.2MB = 30.2MB
```

---

## Metadata Replication

The `__meta` partition stores all cluster metadata using **event sourcing**.

### Metadata Operations (Events)

All metadata changes are represented as events:

```rust
pub enum MetadataOp {
    CreateTopic { name: String, config: TopicConfig },
    DeleteTopic { name: String },
    UpdatePartitionOffset { topic: String, partition: u32, high_watermark: i64 },
    CreateConsumerGroup { group_id: String },
    CommitOffset { group_id: String, topic: String, partition: u32, offset: i64 },
    // ... more operations
}
```

### Event Sourcing Flow

```
┌─────────────────────────────────────────────────────────────┐
│  Metadata State Machine (in-memory)                         │
│                                                               │
│  Current State:                                              │
│    Topics: { "orders": {...}, "users": {...} }              │
│    Consumer Groups: { "group1": {...} }                     │
│    Partition Offsets: { "orders-0": 12345 }                 │
└────────────────────┬────────────────────────────────────────┘
                     │
                     │ Apply event
                     │
        ┌────────────▼────────────┐
        │ Event: CreateTopic      │
        │   name: "payments"      │
        │   partitions: 3         │
        └────────────┬────────────┘
                     │
                     │ Raft replication
                     │
        ┌────────────▼────────────┐
        │ Raft Log Entry          │
        │   index: 10001          │
        │   term: 5               │
        │   data: CreateTopic(...) │
        └────────────┬────────────┘
                     │
                     │ Persist to WAL
                     │
        ┌────────────▼────────────┐
        │ __meta/0/wal_0_1.log    │
        │   (durable storage)     │
        └─────────────────────────┘
```

**Recovery:**
1. Load latest snapshot (if exists)
2. Replay all Raft log entries > snapshot index
3. Apply each event to rebuild metadata state
4. Metadata now fully restored

---

## Snapshot Mechanism

### Snapshot Creation

Triggered when:
- Raft log reaches `snapshot_threshold` entries (e.g., 10,000)
- Manual trigger via admin API
- Background loop (every N minutes)

**Snapshot process:**
1. **Serialize state machine**: Convert in-memory state to bytes
2. **Compress**: Apply gzip or zstd compression
3. **Upload to S3**: Store snapshot in object storage
4. **Truncate log**: Delete Raft log entries ≤ snapshot index
5. **Update metadata**: Record snapshot index/term

### Snapshot Format

```
┌────────────────────────────────────────────────────┐
│  Snapshot File (snapshot_12345.bin.gz)             │
├────────────────────────────────────────────────────┤
│  Header:                                            │
│    Magic: 0x43485253 (CHRS)                        │
│    Version: 1                                       │
│    Last Included Index: 12345                      │
│    Last Included Term: 5                           │
│    Checksum: 0xABCD1234                            │
│                                                     │
│  Data (compressed):                                 │
│    Serialized state machine (bincode)              │
│      - Topics: Vec<TopicMetadata>                  │
│      - Consumer Groups: Vec<ConsumerGroupMetadata> │
│      - Partition Offsets: HashMap<(topic,part),i64>│
└────────────────────────────────────────────────────┘
```

### Snapshot Application (Recovery)

```bash
# Node starts with empty state
# 1. Download latest snapshot from S3
aws s3 cp s3://chronik-prod/snapshots/__meta/0/snapshot_12345.bin.gz /tmp/

# 2. Decompress and deserialize
gunzip snapshot_12345.bin.gz
# State machine now at index 12345

# 3. Replay log entries > 12345
# WAL: wal_0_1.log (entries 12346-12500)
# Apply each entry to state machine

# 4. State machine now at index 12500 (current)
```

**Performance:**
- Snapshot creation: 1-10s (depending on size)
- Snapshot application: 0.5-5s
- Recovery time: Snapshot + log replay = 5-60s (vs. full replay: minutes-hours)

---

## Multi-Partition Design

### Independent Raft Groups

Each partition is a separate Raft group with independent:
- **Leader**: Partition `orders-0` leader may be Node 1, `orders-1` leader may be Node 2
- **Log**: Each partition has its own Raft log
- **State machine**: Partition data isolated

**Advantages:**
1. **Load balancing**: Leaders distributed across nodes
2. **Scalability**: Partitions can scale independently
3. **Fault isolation**: One partition failure doesn't affect others

### Partition Assignment

When a topic is created with 3 partitions and replication factor 3:

```
Topic: orders, Partitions: 3, RF: 3

Assignment:
  Partition 0: [Node 1 (L), Node 2, Node 3]
  Partition 1: [Node 2 (L), Node 3, Node 1]
  Partition 2: [Node 3 (L), Node 1, Node 2]

(L) = Leader
```

**Leader distribution:**
- Node 1: Leader for partition 0
- Node 2: Leader for partition 1
- Node 3: Leader for partition 2

**Benefit:** Even load distribution

### Cross-Partition Operations

Some operations span multiple partitions:
- **Topic creation**: Metadata operation, goes through `__meta` Raft group
- **Produce to multiple partitions**: Independent Raft proposes, parallelized
- **Consumer group rebalance**: Metadata operation

**Example: Produce 100 messages across 3 partitions**
```
Producer → Partition 0 (Node 1 Raft) → Committed (30 msgs)
        ↘ Partition 1 (Node 2 Raft) → Committed (35 msgs)
        ↘ Partition 2 (Node 3 Raft) → Committed (35 msgs)

All 3 Raft groups run in parallel, no coordination needed.
```

---

## See Also

- [RAFT_DEPLOYMENT_GUIDE.md](RAFT_DEPLOYMENT_GUIDE.md) - Cluster setup
- [RAFT_CONFIGURATION_REFERENCE.md](RAFT_CONFIGURATION_REFERENCE.md) - Config tuning
- [RAFT_TROUBLESHOOTING.md](RAFT_TROUBLESHOOTING.md) - Common issues
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) - S3 backup and recovery
