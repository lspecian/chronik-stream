# Chronik Raft Clustering Architecture

This document describes the architectural design of Chronik's distributed consensus system built on top of the OpenRaft library.

## Table of Contents

- [Overview](#overview)
- [Multi-Raft Design](#multi-raft-design)
- [WAL as RaftLogStorage](#wal-as-raftlogstorage)
- [Architecture Diagrams](#architecture-diagrams)
- [Component Interactions](#component-interactions)
- [State Machine Design](#state-machine-design)
- [Network Layer](#network-layer)

## Overview

Chronik's clustering implementation uses **Multi-Raft** consensus, where each topic partition is managed by an independent Raft group. This design provides:

- **Partition-level isolation**: Failures in one partition don't affect others
- **Horizontal scalability**: Add partitions without increasing consensus latency
- **Optimal resource utilization**: Each partition independently manages its replication
- **Simplified recovery**: Partition-level snapshots and log compaction

**Key Components**:
- `RaftNode` - Per-partition Raft instance
- `RaftCluster` - Cluster membership and node discovery
- `RaftLogStorage` - WAL-backed Raft log persistence
- `RaftStateMachine` - Topic partition state machine
- `RaftNetwork` - gRPC-based inter-node communication

## Multi-Raft Design

### Design Decision

**Why Multi-Raft instead of Single-Raft?**

| Aspect | Single-Raft | Multi-Raft (Chosen) |
|--------|-------------|---------------------|
| **Scalability** | Limited by single log | Linear with partitions |
| **Fault Isolation** | Single point of failure | Partition-level isolation |
| **Throughput** | Bottlenecked by leader | Parallel replication |
| **Recovery Time** | Full cluster replay | Per-partition replay |
| **Complexity** | Simpler coordination | More Raft instances |

**Real-world analogy**: Multi-Raft is like having separate file cabinets (partitions) with their own locks (Raft groups), rather than one master lock controlling all files.

### Partition-to-Raft Mapping

```
Topic: "orders" (3 partitions, RF=3)
  ├─ Partition 0 → RaftGroup[orders-0] (Leader: Node1, Followers: Node2, Node3)
  ├─ Partition 1 → RaftGroup[orders-1] (Leader: Node2, Followers: Node1, Node3)
  └─ Partition 2 → RaftGroup[orders-2] (Leader: Node3, Followers: Node1, Node2)

Topic: "payments" (2 partitions, RF=3)
  ├─ Partition 0 → RaftGroup[payments-0] (Leader: Node1, Followers: Node2, Node3)
  └─ Partition 1 → RaftGroup[payments-1] (Leader: Node2, Followers: Node1, Node3)

Result: 5 independent Raft groups across 3 nodes
```

Each Raft group:
- Maintains its own log (WAL)
- Elects its own leader independently
- Replicates only its partition's data
- Can snapshot and compact independently

### Leadership Distribution

Chronik automatically distributes partition leadership across nodes for load balancing:

```
┌─────────────────────────────────────────────────────────────┐
│                    3-Node Cluster                            │
├─────────────────────────────────────────────────────────────┤
│  Node 1 (ID: 1)          Node 2 (ID: 2)      Node 3 (ID: 3) │
│  ┌──────────────┐       ┌──────────────┐    ┌──────────────┐│
│  │ orders-0 (L) │       │ orders-1 (L) │    │ orders-2 (L) ││
│  │ orders-1 (F) │       │ orders-2 (F) │    │ orders-0 (F) ││
│  │ orders-2 (F) │       │ orders-0 (F) │    │ orders-1 (F) ││
│  │              │       │              │    │              ││
│  │ payments-0(L)│       │ payments-1(L)│    │              ││
│  │ payments-1(F)│       │ payments-0(F)│    │ payments-0(F)││
│  │              │       │              │    │ payments-1(F)││
│  └──────────────┘       └──────────────┘    └──────────────┘│
│  L=Leader, F=Follower                                        │
└─────────────────────────────────────────────────────────────┘
```

## WAL as RaftLogStorage

### Design Rationale

Chronik reuses the existing **GroupCommitWal** as the storage backend for Raft logs instead of implementing a separate Raft-specific storage layer.

**Benefits**:

1. **Code Reuse**: Leverage battle-tested WAL implementation
2. **Single Durability Path**: No duplication of fsync/flush logic
3. **Consistent Recovery**: Same WAL recovery mechanism for all data
4. **Performance**: Amortized fsync via group commit
5. **Unified Monitoring**: One set of metrics for all persistence

**Trade-offs**:

| Aspect | Consideration | Mitigation |
|--------|--------------|------------|
| **Schema Mismatch** | WAL has message-specific fields | Wrap Raft entries in `WalRecord::RaftEntry` variant |
| **Compaction** | Different compaction needs | Partition-aware compaction strategies |
| **Recovery** | Must distinguish Raft vs message logs | WAL directory structure: `data/wal/{topic}/{partition}/` |

### WAL Directory Structure

```
data/wal/
├── __meta/                          # Metadata WAL (single Raft group)
│   ├── raft_log_00000001.wal       # Raft entries for cluster metadata
│   ├── raft_log_00000002.wal
│   └── raft_snapshot_00000100.snap
├── orders/
│   ├── 0/
│   │   ├── raft_log_00000001.wal   # Raft entries for orders-0
│   │   ├── raft_log_00000002.wal
│   │   └── messages_00000001.wal    # Actual message data (separate)
│   ├── 1/
│   │   ├── raft_log_00000001.wal   # Raft entries for orders-1
│   │   └── messages_00000001.wal
│   └── 2/
│       ├── raft_log_00000001.wal   # Raft entries for orders-2
│       └── messages_00000001.wal
└── payments/
    ├── 0/
    │   ├── raft_log_00000001.wal
    │   └── messages_00000001.wal
    └── 1/
        ├── raft_log_00000001.wal
        └── messages_00000001.wal
```

**Key Points**:
- Each partition has TWO WAL types: `raft_log_*.wal` (consensus) and `messages_*.wal` (data)
- Raft WAL stores: `EntryPayload<LogEntry>` (append requests, snapshots, config changes)
- Message WAL stores: `CanonicalRecord` (actual Kafka messages)
- Metadata partition (`__meta`) uses Raft for cluster config, topic creation, etc.

### RaftLogStorage Implementation

```rust
pub struct WalRaftLogStorage {
    wal: Arc<GroupCommitWal>,
    snapshot_manager: Arc<RaftSnapshotManager>,
}

impl RaftLogStorage for WalRaftLogStorage {
    async fn save_vote(&mut self, vote: &Vote) -> Result<()> {
        // Persist vote to WAL with immediate fsync (critical for safety)
        let entry = WalRecord::RaftVote { vote: vote.clone() };
        self.wal.append_with_fsync(entry).await
    }

    async fn read_vote(&self) -> Result<Option<Vote>> {
        // Read latest vote from WAL
        self.wal.read_last_vote().await
    }

    async fn append_to_log(&mut self, entries: &[Entry<LogEntry>]) -> Result<()> {
        // Convert Raft entries to WalRecords
        let wal_entries: Vec<WalRecord> = entries
            .iter()
            .map(|e| WalRecord::RaftEntry {
                index: e.log_id.index,
                term: e.log_id.leader_id.term,
                payload: e.payload.clone(),
            })
            .collect();

        // Use group commit (amortized fsync)
        self.wal.append_batch(wal_entries).await
    }

    async fn apply_to_state_machine(&mut self, entries: &[Entry<LogEntry>]) -> Result<()> {
        // Apply committed entries to partition state machine
        for entry in entries {
            match &entry.payload {
                EntryPayload::Normal(log_entry) => {
                    self.state_machine.apply(log_entry).await?;
                }
                EntryPayload::Membership(config) => {
                    self.state_machine.apply_config(config).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn get_log_state(&self) -> Result<LogState> {
        // Return last purged and last log ID
        Ok(LogState {
            last_purged_log_id: self.wal.get_first_index().await?,
            last_log_id: self.wal.get_last_index().await?,
        })
    }
}
```

## Architecture Diagrams

### High-Level System Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Chronik Cluster                              │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────┐    │
│  │                     Client Layer                            │    │
│  │  Kafka Producer/Consumer (kafka-python, Java client, etc.)  │    │
│  └─────────────────────┬───────────────────────────────────────┘    │
│                        │ Kafka Protocol (TCP 9092)                   │
│  ┌─────────────────────▼───────────────────────────────────────┐    │
│  │                  KafkaProtocolHandler                        │    │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │    │
│  │  │   Produce    │  │    Fetch     │  │   Metadata   │      │    │
│  │  │   Handler    │  │   Handler    │  │   Handler    │      │    │
│  │  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │    │
│  └─────────┼──────────────────┼──────────────────┼─────────────┘    │
│            │                  │                  │                   │
│  ┌─────────▼──────────────────▼──────────────────▼─────────────┐    │
│  │                    RaftCluster                               │    │
│  │  ┌────────────────────────────────────────────────────────┐ │    │
│  │  │         Partition Routing & Leadership Discovery       │ │    │
│  │  └────────────────────────┬───────────────────────────────┘ │    │
│  │                           │                                  │    │
│  │  ┌────────────────┬───────▼───────┬────────────────┐        │    │
│  │  │ RaftNode       │ RaftNode      │ RaftNode       │        │    │
│  │  │ (orders-0)     │ (orders-1)    │ (payments-0)   │        │    │
│  │  │                │               │                │        │    │
│  │  │ ┌────────────┐ │ ┌────────────┐│ ┌────────────┐│        │    │
│  │  │ │StateMachine│ │ │StateMachine││ │StateMachine││        │    │
│  │  │ │(Partition) │ │ │(Partition) ││ │(Partition) ││        │    │
│  │  │ └─────┬──────┘ │ └─────┬──────┘│ └─────┬──────┘│        │    │
│  │  │       │        │       │       │       │       │        │    │
│  │  │ ┌─────▼──────┐ │ ┌─────▼──────┐│ ┌─────▼──────┐│        │    │
│  │  │ │  WAL       │ │ │  WAL       ││ │  WAL       ││        │    │
│  │  │ │  Storage   │ │ │  Storage   ││ │  Storage   ││        │    │
│  │  │ └────────────┘ │ └────────────┘│ └────────────┘│        │    │
│  │  └────────────────┴───────────────┴────────────────┘        │    │
│  └───────────────────────────────────────────────────────────┘    │
│                                                                      │
│  ┌───────────────────────────────────────────────────────────┐    │
│  │                  RaftNetwork (gRPC)                        │    │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐    │    │
│  │  │ AppendEntries│  │ RequestVote  │  │ InstallSnapshot    │    │
│  │  └──────────────┘  └──────────────┘  └──────────────┘    │    │
│  └───────────────────────────────────────────────────────────┘    │
│                           │                                          │
│                           │ gRPC (TCP 9093)                          │
│                           ▼                                          │
│              ┌────────────────────────────┐                         │
│              │   Peer Nodes (Replicas)    │                         │
│              └────────────────────────────┘                         │
└─────────────────────────────────────────────────────────────────────┘
```

### Write Path (Produce)

```
Producer
   │
   │ 1. ProduceRequest (topic=orders, partition=0, messages=[...])
   │
   ▼
KafkaProtocolHandler
   │
   │ 2. Parse request, extract partition
   │
   ▼
RaftCluster.route_produce(topic, partition)
   │
   │ 3. Find leader for orders-0 RaftGroup
   │ 4. If not leader, return NOT_LEADER_FOR_PARTITION error
   │ 5. If leader, proceed
   │
   ▼
RaftNode[orders-0].propose_write(messages)
   │
   │ 6. Create Raft log entry: LogEntry::Messages { records: [...] }
   │ 7. Call raft.client_write(entry)
   │
   ▼
OpenRaft (orders-0 leader)
   │
   │ 8. Append entry to local Raft log (WAL)
   │ 9. Replicate to followers via RaftNetwork.append_entries()
   │
   ├──────────────┬──────────────┐
   │              │              │
   ▼              ▼              ▼
Follower1    Follower2      Follower3
   │              │              │
   │ 10. Append to local WAL     │
   │ 11. Send ACK                │
   │              │              │
   └──────────────┴──────────────┘
                  │
                  ▼
OpenRaft (orders-0 leader)
   │
   │ 12. Quorum achieved (majority ACKs)
   │ 13. Mark entry as committed
   │ 14. Apply to state machine
   │
   ▼
PartitionStateMachine.apply(entry)
   │
   │ 15. Write messages to message WAL
   │ 16. Update high watermark
   │ 17. Return success
   │
   ▼
ProduceResponse (offset=12345, timestamp=...)
   │
   ▼
Producer
```

### Read Path (Fetch)

```
Consumer
   │
   │ 1. FetchRequest (topic=orders, partition=0, offset=100)
   │
   ▼
KafkaProtocolHandler
   │
   │ 2. Parse request
   │
   ▼
RaftCluster.route_fetch(topic, partition, offset)
   │
   │ 3. Find leader for orders-0 RaftGroup
   │ 4. Check if local node is leader or follower
   │
   ├─────────────┬─────────────┐
   │             │             │
   ▼             ▼             ▼
Leader      Follower     Follower
   │             │             │
   │ 5a. Read   │ 5b. Redirect│
   │ directly   │ to leader   │
   │            │             │
   ▼            │             │
PartitionStateMachine.read(offset)
   │
   │ 6. Read from message WAL (Tier 1)
   │ 7. If not found, check segments (Tier 2)
   │ 8. If not found, check S3 archives (Tier 3)
   │
   ▼
FetchResponse (messages=[...])
   │
   ▼
Consumer
```

### Leader Election Flow

```
Scenario: orders-0 leader (Node1) fails

Time T0: Normal Operation
┌────────┐       ┌────────┐       ┌────────┐
│ Node1  │       │ Node2  │       │ Node3  │
│ Leader │◄─────►│Follower│◄─────►│Follower│
│ (alive)│       │        │       │        │
└────────┘       └────────┘       └────────┘

Time T1: Leader Fails
┌────────┐       ┌────────┐       ┌────────┐
│ Node1  │       │ Node2  │       │ Node3  │
│  XXX   │       │Follower│◄─────►│Follower│
│ (dead) │       │        │       │        │
└────────┘       └────────┘       └────────┘
                      │               │
                      │ 1. Heartbeat  │
                      │    timeout    │
                      │ 2. Transition │
                      │    to         │
                      │    Candidate  │
                      ▼               ▼

Time T2: Election Triggered
┌────────┐       ┌────────┐       ┌────────┐
│ Node1  │       │ Node2  │       │ Node3  │
│  XXX   │       │Candidate      │Follower│
│        │       │        │──────►│        │
└────────┘       └────────┘       └────────┘
                      │ 3. RequestVote │
                      │    (term=2)    │
                      │◄───────────────┤
                      │ 4. VoteGranted │

Time T3: New Leader Elected
┌────────┐       ┌────────┐       ┌────────┐
│ Node1  │       │ Node2  │       │ Node3  │
│  XXX   │       │ Leader │◄─────►│Follower│
│        │       │ (new)  │       │        │
└────────┘       └────────┘       └────────┘
                      │
                      │ 5. Update metadata
                      │ 6. Resume serving writes
                      ▼
```

## Component Interactions

### RaftNode Lifecycle

```rust
// 1. Node Startup
async fn start_raft_node(
    node_id: u64,
    topic: &str,
    partition: i32,
    config: RaftConfig,
) -> Result<RaftNode> {
    // Create WAL-backed log storage
    let log_storage = WalRaftLogStorage::new(
        &format!("data/wal/{}/{}", topic, partition)
    ).await?;

    // Create partition state machine
    let state_machine = PartitionStateMachine::new(topic, partition).await?;

    // Create Raft instance
    let raft = Raft::new(
        node_id,
        config,
        Arc::new(RaftNetwork::new()),
        log_storage,
        state_machine,
    ).await?;

    // Initialize from WAL (recovery)
    raft.initialize(Vec::new()).await?;

    Ok(RaftNode { raft, topic, partition })
}

// 2. Join Cluster (for new node)
async fn join_cluster(&self, leader_node_id: u64) -> Result<()> {
    // Send AddLearner request to leader
    let response = self.network
        .add_learner(leader_node_id, self.node_id)
        .await?;

    // Wait for log replication to catch up
    self.raft.wait_for_snapshot().await?;

    // Send ChangeMembership to promote to voter
    self.raft
        .change_membership(vec![leader_node_id, self.node_id], true)
        .await?;

    Ok(())
}

// 3. Serving Writes (leader only)
async fn propose_write(&self, entry: LogEntry) -> Result<ApplyResponse> {
    if !self.is_leader().await {
        return Err(Error::NotLeader);
    }

    let response = self.raft.client_write(entry).await?;
    Ok(response)
}

// 4. Serving Reads (leader or follower with linearizable read)
async fn read(&self, offset: i64) -> Result<Vec<CanonicalRecord>> {
    // Option 1: Linearizable read (always up-to-date)
    self.raft.ensure_linearizable_read().await?;

    // Option 2: Read from local state machine (may be stale)
    self.state_machine.read(offset).await
}

// 5. Snapshot Creation
async fn create_snapshot(&self) -> Result<()> {
    let snapshot_data = self.state_machine.snapshot().await?;

    let snapshot = Snapshot {
        meta: SnapshotMeta {
            last_log_id: self.raft.metrics().await.last_log_index,
            last_membership: self.raft.metrics().await.membership_config,
        },
        data: snapshot_data,
    };

    self.snapshot_manager.save(snapshot).await?;
    Ok(())
}
```

### Cross-Component Flow Example: Topic Creation

```
Client
   │
   │ 1. CreateTopicsRequest (topic=orders, partitions=3, RF=3)
   │
   ▼
KafkaProtocolHandler.handle_create_topics()
   │
   │ 2. Validate request
   │
   ▼
RaftCluster.create_topic(topic_config)
   │
   │ 3. Create Raft log entry: LogEntry::CreateTopic { ... }
   │ 4. Propose to metadata Raft group (__meta partition)
   │
   ▼
RaftNode[__meta].propose_write(entry)
   │
   │ 5. Raft consensus (replicate to quorum)
   │ 6. Apply to metadata state machine
   │
   ▼
MetadataStateMachine.apply(CreateTopic)
   │
   │ 7. Create topic metadata
   │ 8. For each partition (0, 1, 2):
   │    a. Allocate partition to nodes (round-robin)
   │    b. Initialize RaftNode for partition
   │    c. Bootstrap partition Raft group
   │
   ├──────────────┬──────────────┬──────────────┐
   │              │              │              │
   ▼              ▼              ▼              ▼
RaftNode     RaftNode     RaftNode     RaftNode
(orders-0)   (orders-1)   (orders-2)   (__meta)
   │              │              │
   │ 9. Initialize empty WAL     │
   │ 10. Join cluster membership │
   │ 11. Elect leader            │
   │              │              │
   └──────────────┴──────────────┘
                  │
                  ▼
CreateTopicsResponse (success)
   │
   ▼
Client
```

## State Machine Design

### PartitionStateMachine

The state machine for each partition manages:

1. **Message Storage**: Append messages to partition WAL
2. **Offset Management**: Track high watermark and last stable offset
3. **Index Management**: Maintain offset-to-position index
4. **Snapshot Creation**: Serialize partition state for Raft snapshots

```rust
pub struct PartitionStateMachine {
    topic: String,
    partition: i32,

    // Message storage (separate from Raft log)
    message_wal: Arc<GroupCommitWal>,

    // In-memory index (offset → WAL position)
    offset_index: BTreeMap<i64, WalPosition>,

    // Partition metadata
    high_watermark: AtomicI64,
    last_stable_offset: AtomicI64,

    // Snapshot manager
    snapshot_manager: Arc<PartitionSnapshotManager>,
}

impl RaftStateMachine for PartitionStateMachine {
    async fn apply(&mut self, log_entry: LogEntry) -> Result<ApplyResponse> {
        match log_entry {
            LogEntry::Messages { records } => {
                // Append messages to message WAL
                let start_offset = self.high_watermark.load(Ordering::SeqCst);

                for (i, record) in records.iter().enumerate() {
                    let offset = start_offset + i as i64;
                    let position = self.message_wal.append(record).await?;
                    self.offset_index.insert(offset, position);
                }

                // Update high watermark
                let new_hwm = start_offset + records.len() as i64;
                self.high_watermark.store(new_hwm, Ordering::SeqCst);

                Ok(ApplyResponse::Messages { last_offset: new_hwm - 1 })
            }

            LogEntry::Compact { before_offset } => {
                // Compact message WAL up to offset
                self.message_wal.compact_before(before_offset).await?;
                self.offset_index.retain(|&k, _| k >= before_offset);
                Ok(ApplyResponse::Compacted)
            }
        }
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        // Create partition snapshot
        let snapshot = PartitionSnapshot {
            high_watermark: self.high_watermark.load(Ordering::SeqCst),
            last_stable_offset: self.last_stable_offset.load(Ordering::SeqCst),
            offset_index: self.offset_index.clone(),
            message_wal_state: self.message_wal.snapshot_state().await?,
        };

        // Serialize to bytes
        let data = bincode::serialize(&snapshot)?;

        Ok(Snapshot {
            meta: SnapshotMeta { ... },
            data,
        })
    }

    async fn install_snapshot(&mut self, snapshot: Snapshot) -> Result<()> {
        // Deserialize snapshot
        let partition_snapshot: PartitionSnapshot = bincode::deserialize(&snapshot.data)?;

        // Restore state
        self.high_watermark.store(partition_snapshot.high_watermark, Ordering::SeqCst);
        self.last_stable_offset.store(partition_snapshot.last_stable_offset, Ordering::SeqCst);
        self.offset_index = partition_snapshot.offset_index;

        // Restore message WAL state
        self.message_wal.restore_from_snapshot(partition_snapshot.message_wal_state).await?;

        Ok(())
    }
}
```

### MetadataStateMachine

The metadata state machine manages cluster-wide metadata:

```rust
pub struct MetadataStateMachine {
    // Topic metadata
    topics: HashMap<String, TopicMetadata>,

    // Partition assignments (topic, partition) → [node_ids]
    partition_assignments: HashMap<(String, i32), Vec<u64>>,

    // Cluster membership
    nodes: HashMap<u64, NodeInfo>,
}

impl RaftStateMachine for MetadataStateMachine {
    async fn apply(&mut self, log_entry: LogEntry) -> Result<ApplyResponse> {
        match log_entry {
            LogEntry::CreateTopic { name, config } => {
                // Create topic metadata
                self.topics.insert(name.clone(), TopicMetadata {
                    name: name.clone(),
                    partitions: config.num_partitions,
                    replication_factor: config.replication_factor,
                    config,
                });

                // Assign partitions to nodes (round-robin)
                for partition in 0..config.num_partitions {
                    let nodes = self.assign_partition_replicas(
                        &name,
                        partition,
                        config.replication_factor,
                    );
                    self.partition_assignments.insert((name.clone(), partition), nodes);
                }

                Ok(ApplyResponse::TopicCreated)
            }

            LogEntry::DeleteTopic { name } => {
                self.topics.remove(&name);
                self.partition_assignments.retain(|(t, _), _| t != &name);
                Ok(ApplyResponse::TopicDeleted)
            }

            LogEntry::AddNode { node_id, address } => {
                self.nodes.insert(node_id, NodeInfo { node_id, address });
                Ok(ApplyResponse::NodeAdded)
            }

            LogEntry::RemoveNode { node_id } => {
                self.nodes.remove(&node_id);
                // TODO: Rebalance partitions
                Ok(ApplyResponse::NodeRemoved)
            }
        }
    }
}
```

## Network Layer

### gRPC Service Definition

```protobuf
service RaftService {
    // Core Raft RPCs
    rpc AppendEntries(AppendEntriesRequest) returns (AppendEntriesResponse);
    rpc RequestVote(RequestVoteRequest) returns (RequestVoteResponse);
    rpc InstallSnapshot(stream InstallSnapshotRequest) returns (InstallSnapshotResponse);

    // Client operations
    rpc ProposeWrite(ProposeRequest) returns (ProposeResponse);
    rpc QueryRead(QueryRequest) returns (QueryResponse);

    // Membership changes
    rpc AddLearner(AddLearnerRequest) returns (AddLearnerResponse);
    rpc ChangeMembership(ChangeMembershipRequest) returns (ChangeMembershipResponse);
}
```

### RaftNetwork Implementation

```rust
pub struct RaftNetwork {
    clients: Arc<RwLock<HashMap<u64, RaftServiceClient<Channel>>>>,
    node_addresses: Arc<RwLock<HashMap<u64, String>>>,
}

impl RaftNetworkAPI for RaftNetwork {
    async fn append_entries(
        &self,
        target_node: u64,
        request: AppendEntriesRequest,
    ) -> Result<AppendEntriesResponse> {
        let client = self.get_or_create_client(target_node).await?;
        let response = client.append_entries(request).await?.into_inner();
        Ok(response)
    }

    async fn install_snapshot(
        &self,
        target_node: u64,
        request: InstallSnapshotRequest,
    ) -> Result<InstallSnapshotResponse> {
        let client = self.get_or_create_client(target_node).await?;

        // Stream snapshot data in chunks (1MB each)
        let chunks = request.data.chunks(1024 * 1024);
        let stream = stream::iter(chunks.map(|chunk| {
            InstallSnapshotChunk {
                snapshot_id: request.snapshot_id,
                offset: chunk.as_ptr() as u64,
                data: chunk.to_vec(),
                done: false,
            }
        }));

        let response = client.install_snapshot(stream).await?.into_inner();
        Ok(response)
    }

    async fn vote(
        &self,
        target_node: u64,
        request: RequestVoteRequest,
    ) -> Result<RequestVoteResponse> {
        let client = self.get_or_create_client(target_node).await?;
        let response = client.request_vote(request).await?.into_inner();
        Ok(response)
    }
}
```

## Performance Considerations

### Optimizations

1. **Batching**: Group commit for Raft log appends (via GroupCommitWal)
2. **Pipeline Replication**: OpenRaft supports pipelined AppendEntries
3. **Read Optimization**: Followers can serve stale reads without leader communication
4. **Snapshot Streaming**: Large snapshots transmitted in chunks
5. **Log Compaction**: Automatic compaction when log size exceeds threshold

### Tuning Parameters

```toml
[cluster.raft]
# Election timeout (150-300ms recommended)
election_timeout_ms = 200

# Heartbeat interval (1/10 of election timeout)
heartbeat_interval_ms = 20

# Max entries per AppendEntries RPC
max_payload_entries = 1000

# Snapshot threshold (trigger snapshot after N log entries)
snapshot_log_size_threshold = 100000

# Log compaction threshold
log_compaction_threshold = 50000
```

## References

- **OpenRaft Documentation**: https://docs.rs/openraft/
- **Raft Paper**: https://raft.github.io/raft.pdf
- **Implementation Plan**: [Implementation plan document location]
- **Configuration Guide**: [docs/raft/CONFIGURATION.md](./CONFIGURATION.md)
- **Troubleshooting Guide**: [docs/raft/TROUBLESHOOTING.md](./TROUBLESHOOTING.md)
