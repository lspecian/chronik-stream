# Clean Raft Implementation: Build from v2.2.0 Branch

**Base Branch**: `feat/v2.2.0-clean-wal-replication` (THIS branch, 52K msg/s proven)
**Target**: Kafka-compatible cluster with 60K+ msg/s standalone, 45K+ with replication
**Strategy**: Incremental additions with benchmarking at EVERY step

---

## Starting Point: What We Have

✅ **Proven Performance**: 52K msg/s standalone, 28K msg/s with 1 follower
✅ **Zero-Copy WAL Replication**: No double-parsing, direct bincode streaming
✅ **Lock-Free Architecture**: No RwLock bottlenecks
✅ **Clean Codebase**: No technical debt, no mysterious slowness

---

## Phase 1: Per-Partition WAL Files (2-3 days)

### Goal
Refactor WAL to support partition-level operations (prerequisite for Raft partition replication)

### Current Structure
```
data/wal/
  ├─ __meta/              (metadata)
  └─ {topic}/
      └─ partition-{N}/   (all in one WAL instance)
```

### Target Structure
```
data/wal/
  ├─ __meta/              (cluster metadata)
  └─ partitions/
      └─ {topic}-{partition}/
          ├─ segment_000.wal  (sealed, immutable)
          ├─ segment_001.wal  (active)
          └─ offset_index     (offset → segment mapping)
```

### Implementation Steps

**Step 1.1: Modify WalManager API** (`chronik-wal/src/manager.rs`)
```rust
// BEFORE: Topic-scoped WAL
impl WalManager {
    pub async fn append(&self, topic: &str, partition: i32, data: Vec<u8>) -> Result<()>
}

// AFTER: Partition-scoped WAL
impl WalManager {
    // Get or create WAL for specific partition
    fn get_partition_wal(&self, topic: &str, partition: i32) -> Arc<GroupCommitWal>;
    
    // Each partition has its own GroupCommitWal instance
    partitions: DashMap<PartitionKey, Arc<GroupCommitWal>>,
}

type PartitionKey = (String, i32);  // (topic, partition)
```

**Step 1.2: Update GroupCommitWal** (`chronik-wal/src/group_commit.rs`)
```rust
impl GroupCommitWal {
    // Add partition context
    pub fn new(
        topic: String,
        partition: i32,  // NEW
        config: WalConfig,
    ) -> Result<Self>;
    
    // Path becomes: data/wal/partitions/{topic}-{partition}/segment_XXX.wal
}
```

**Step 1.3: Update ProduceHandler** (`chronik-server/src/produce_handler.rs`)
```rust
// BEFORE: One WAL manager for all partitions
wal_mgr.append(topic, partition, data).await

// AFTER: Per-partition WAL instances (cleaner, enables partition reassignment)
let partition_wal = wal_mgr.get_partition_wal(topic, partition);
partition_wal.append(data).await
```

### Testing & Benchmarking

```bash
# Build
cargo build --release --bin chronik-server

# Benchmark (MUST match baseline!)
./target/release/chronik-bench --bootstrap-servers localhost:9092 \
  --duration 30 --concurrency 128 --message-size 256

# Success Criteria: >= 50K msg/s (within 5% of 52K baseline)
```

### Cherry-Pick from v2.4.1
- None needed - this is new architecture

### Deliverable
- ✅ Per-partition WAL files working
- ✅ Performance maintained (50K+ msg/s)
- ✅ Tests passing
- ✅ Commit: `feat(v2.5.0): Phase 1 - Per-partition WAL files`

---

## Phase 2: Add Raft for Metadata Only (2 days)

### Goal
Add Raft consensus for cluster metadata (NOT data replication)

### What Raft Manages
- ✅ Cluster membership (which nodes are alive)
- ✅ Partition assignments (partition-0 → [node1, node2, node3])
- ✅ Partition leaders (partition-0 leader = node1)
- ❌ Data replication (WAL streaming handles this)

### Implementation Steps

**Step 2.1: Add Raft Dependencies**
```toml
# Cargo.toml
[dependencies]
raft = "0.7"
raft-proto = "0.7"
prost = "0.12"
```

**Step 2.2: Create Minimal Raft State Machine**
```rust
// crates/chronik-server/src/raft_metadata.rs

#[derive(Serialize, Deserialize)]
pub enum MetadataCommand {
    // Cluster membership
    AddNode { node_id: u64, address: String },
    RemoveNode { node_id: u64 },
    
    // Partition assignments
    AssignPartition { topic: String, partition: i32, replicas: Vec<u64> },
    SetPartitionLeader { topic: String, partition: i32, leader: u64 },
    
    // ISR tracking
    UpdateISR { topic: String, partition: i32, isr: Vec<u64> },
}

pub struct MetadataStateMachine {
    // Current cluster state
    nodes: HashMap<u64, String>,  // node_id -> address
    partition_assignments: HashMap<PartitionKey, Vec<u64>>,  // replicas
    partition_leaders: HashMap<PartitionKey, u64>,  // leader node_id
    isr_sets: HashMap<PartitionKey, Vec<u64>>,  // in-sync replicas
}

impl raft::StateMachine for MetadataStateMachine {
    fn apply(&mut self, entry: &[u8]) -> Result<Vec<u8>> {
        let cmd: MetadataCommand = bincode::deserialize(entry)?;
        match cmd {
            MetadataCommand::AssignPartition { topic, partition, replicas } => {
                self.partition_assignments.insert((topic, partition), replicas);
                Ok(vec![])
            }
            // ... other commands
        }
    }
}
```

**Step 2.3: Initialize Raft Cluster**
```rust
// crates/chronik-server/src/raft_cluster.rs

pub struct RaftCluster {
    node: RawNode<MemStorage>,
    state_machine: Arc<RwLock<MetadataStateMachine>>,
}

impl RaftCluster {
    pub async fn bootstrap(node_id: u64, peers: Vec<(u64, String)>) -> Result<Self> {
        // Create Raft node
        let config = raft::Config { id: node_id, ..Default::default() };
        let storage = MemStorage::new();
        let node = RawNode::new(&config, storage, &raft::default_logger())?;
        
        Ok(Self {
            node,
            state_machine: Arc::new(RwLock::new(MetadataStateMachine::new())),
        })
    }
    
    // Get partition assignment
    pub fn get_partition_replicas(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        let sm = self.state_machine.read().unwrap();
        sm.partition_assignments.get(&(topic.to_string(), partition)).cloned()
    }
}
```

**Step 2.4: Wire to IntegratedServer** (OPTIONAL field, like replication manager)
```rust
// crates/chronik-server/src/integrated_server.rs

pub struct IntegratedKafkaServer {
    // Existing fields...
    raft_cluster: Option<Arc<RaftCluster>>,  // NEW - only for cluster mode
}
```

### Testing & Benchmarking

```bash
# Test standalone (no Raft) - MUST be fast
cargo run --bin chronik-server -- standalone
./target/release/chronik-bench ...
# Success: >= 50K msg/s (no regression!)

# Test with Raft metadata
cargo run --bin chronik-server -- \
  --node-id 1 \
  --raft-addr localhost:5001 \
  --peers "2@localhost:5002,3@localhost:5003" \
  standalone
./target/release/chronik-bench ...
# Success: >= 48K msg/s (within 5% overhead)
```

### Cherry-Pick from v2.4.1
- Raft initialization code
- MetadataStateMachine structure
- **SKIP**: RaftBatchProposer, WAL-Batch-to-Raft (we don't need data in Raft)

### Deliverable
- ✅ Raft cluster for metadata only
- ✅ Standalone mode still fast (50K+)
- ✅ Cluster mode minimal overhead (48K+)
- ✅ Commit: `feat(v2.5.0): Phase 2 - Raft metadata layer`

---

## Phase 3: Partition-Level Replication with ISR (3 days)

### Goal
Replicate specific partitions (not whole WAL) with ISR tracking

### Implementation Steps

**Step 3.1: ISR Tracker**
```rust
// crates/chronik-server/src/isr_tracker.rs

pub struct IsrTracker {
    // Track follower lag per partition
    follower_offsets: DashMap<(u64, PartitionKey), i64>,  // (node_id, partition) -> last_offset
    
    // ISR configuration
    max_lag_ms: u64,
    max_lag_entries: u64,
}

impl IsrTracker {
    pub fn is_in_sync(&self, node_id: u64, topic: &str, partition: i32) -> bool {
        // Check if follower is caught up
        let leader_offset = self.get_leader_offset(topic, partition);
        let follower_offset = self.follower_offsets
            .get(&(node_id, (topic.to_string(), partition)))
            .map(|v| *v)
            .unwrap_or(0);
        
        let lag = leader_offset - follower_offset;
        lag < self.max_lag_entries as i64
    }
    
    pub fn update_follower_offset(&self, node_id: u64, topic: &str, partition: i32, offset: i64) {
        self.follower_offsets.insert((node_id, (topic.to_string(), partition)), offset);
    }
}
```

**Step 3.2: Modify Replication Manager for Partition-Level Replication**
```rust
// crates/chronik-server/src/wal_replication.rs

impl WalReplicationManager {
    pub async fn replicate_partition(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        data: Vec<u8>,
    ) {
        // Get replicas for this partition from Raft metadata
        let replicas = self.raft_cluster.get_partition_replicas(&topic, partition)?;
        
        // Only send to in-sync replicas
        for replica_node in replicas {
            if self.isr_tracker.is_in_sync(replica_node, &topic, partition) {
                self.send_to_node(replica_node, topic.clone(), partition, offset, data.clone()).await;
            }
        }
    }
}
```

**Step 3.3: Update Produce Path**
```rust
// crates/chronik-server/src/produce_handler.rs

// After WAL write
if let Some(ref repl_mgr) = self.wal_replication_manager {
    // Only replicate if this partition has replicas assigned
    if let Some(replicas) = self.get_partition_replicas(topic, partition) {
        if replicas.len() > 1 {  // Has followers
            repl_mgr.replicate_partition(
                topic.to_string(),
                partition,
                offset,
                serialized_data,
            ).await;
        }
    }
}
```

### Testing & Benchmarking

```bash
# 3-node cluster, partition has 3 replicas
# Node 1: Leader for partition-0
# Node 2: Follower for partition-0
# Node 3: Follower for partition-0

# Benchmark leader
./target/release/chronik-bench --bootstrap-servers node1:9092 ...
# Success: >= 45K msg/s (acceptable overhead for 2 followers)
```

### Cherry-Pick from v2.4.1
- ISR tracking logic (`isr.rs`)
- Partition assignment queries

### Deliverable
- ✅ Partition-level replication working
- ✅ ISR tracking working
- ✅ Performance target met (45K+ msg/s)
- ✅ Commit: `feat(v2.5.0): Phase 3 - Partition replication + ISR`

---

## Phase 4: Separate acks=-1 Path (1 day)

### Goal
Implement your brilliant idea: fast path for acks=0/1, slow path for acks=-1

### Implementation

```rust
// crates/chronik-server/src/produce_handler.rs

pub async fn produce_to_partition(..., acks: i16) -> Result<i64> {
    // Write to local WAL (always, for durability)
    let offset = self.wal_write(topic, partition, batch).await?;
    
    match acks {
        0 => {
            // FAST PATH: Fire-and-forget
            // Replication happens in background (existing code)
            Ok(offset)
        }
        1 => {
            // FAST PATH: Local fsync only (existing code)
            // Replication happens in background
            Ok(offset)
        }
        -1 => {
            // SLOW PATH: Wait for ISR quorum
            let (tx, rx) = oneshot::channel();
            
            // Send to ISR quorum buffer
            self.isr_ack_tracker.register_wait(topic, partition, offset, tx);
            
            // Wait for quorum (with timeout)
            tokio::time::timeout(
                Duration::from_secs(30),
                rx
            ).await??
        }
    }
}

// Follower acknowledges replication
pub async fn handle_follower_ack(&self, topic: &str, partition: i32, offset: i64, node_id: u64) {
    self.isr_ack_tracker.record_ack(topic, partition, offset, node_id);
    
    // Check if quorum reached
    if self.isr_ack_tracker.has_quorum(topic, partition, offset) {
        // Notify waiting producer
        self.isr_ack_tracker.notify(topic, partition, offset);
    }
}
```

### Testing & Benchmarking

```bash
# Test acks=0 (fire-and-forget)
# Target: >= 60K msg/s (minimal overhead)

# Test acks=1 (leader fsync)
# Target: >= 55K msg/s (existing baseline)

# Test acks=-1 (ISR quorum)
# Target: >= 40K msg/s (acceptable for strong consistency)
```

### Cherry-Pick from v2.4.1
- None - this is new design

### Deliverable
- ✅ acks=0/1 fast path: 55K+ msg/s
- ✅ acks=-1 slow path: 40K+ msg/s
- ✅ Commit: `feat(v2.5.0): Phase 4 - Separate acks=-1 quorum path`

---

## Phase 5: Leader Election per Partition (1-2 days)

### Goal
Automatic failover when partition leader dies

### Implementation

```rust
// Raft already handles this in metadata state machine
// When leader detects follower is caught up, promote it

impl MetadataStateMachine {
    pub fn elect_partition_leader(&mut self, topic: &str, partition: i32) -> u64 {
        let replicas = self.partition_assignments.get(&(topic.to_string(), partition))?;
        let isr = self.isr_sets.get(&(topic.to_string(), partition))?;
        
        // Pick first ISR member as new leader
        let new_leader = isr.first()?;
        self.partition_leaders.insert((topic.to_string(), partition), *new_leader);
        
        *new_leader
    }
}
```

### Testing
- Kill leader node
- Verify partition fails over to ISR follower
- Verify produce continues to new leader

### Deliverable
- ✅ Automatic partition leader election
- ✅ Commit: `feat(v2.5.0): Phase 5 - Partition leader election`

---

## Success Criteria (Final)

### Performance Targets
- ✅ Standalone (no replication): >= 70K msg/s
- ✅ acks=0/1 (async replication): >= 55K msg/s
- ✅ acks=-1 (quorum): >= 40K msg/s

### Features
- ✅ Per-partition WAL files
- ✅ Raft for metadata coordination
- ✅ Partition-level replication
- ✅ ISR tracking
- ✅ Separate fast/slow producer paths
- ✅ Automatic leader election per partition
- ✅ No RwLock in hot path

### Kafka Compatibility
- ✅ Partition replication (like Kafka ISR)
- ✅ acks=0/1/-1 support
- ✅ Leader election
- ✅ Cluster coordination
- ✅ Fine-grained failover

---

## Timeline

**Total: 9-11 days**

- Phase 1 (Per-partition WAL): 2-3 days
- Phase 2 (Raft metadata): 2 days
- Phase 3 (Partition replication + ISR): 3 days
- Phase 4 (Separate acks=-1 path): 1 day
- Phase 5 (Leader election): 1-2 days

**Delivery**: v2.5.0 - True Kafka-compatible cluster, clean architecture, 60K+ msg/s

---

## Next Steps

**Ready to start Phase 1?**

1. Create new branch from current: `feat/v2.5.0-kafka-cluster`
2. Implement per-partition WAL files
3. Benchmark at EVERY step
4. Cherry-pick only what works from v2.4.1

**Say the word and I'll begin Phase 1 implementation.**
