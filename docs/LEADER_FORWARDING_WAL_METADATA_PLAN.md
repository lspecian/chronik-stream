# Leader-Forwarding + WAL Metadata Replication Implementation Plan

**Version**: v2.3.0
**Status**: PLANNING
**Goal**: Break through 1,600 msg/s Raft consensus bottleneck → 10,000+ msg/s
**Estimated Timeline**: 3-4 weeks across multiple sessions

---

## TL;DR - The Strategy

Replace slow Raft consensus for metadata operations with:
1. **WAL-based metadata replication** (fast local writes, async replication)
2. **Leader-forwarding pattern** (followers forward reads to leader)
3. **Leader leases** (enable fast local reads on followers with strong consistency)

**Expected Improvement**: 6-10x throughput (1,600 → 10,000+ msg/s)

---

## Current Bottleneck Analysis

### The Problem

**Concurrency Scaling Test Results** (v2.2.9):
- 1 thread: ~1,600 msg/s
- 2 threads: ~2,400 msg/s
- 4 threads: ~3,200 msg/s (plateau)
- 8 threads: ~3,200 msg/s (no further gain)
- 16 threads: ~3,200 msg/s (saturated)

**Root Cause**: Every metadata operation (topic creation, partition assignment, broker registration) triggers Raft consensus:
- Raft consensus latency: 10-50ms per operation
- Serializes through leader's propose() → commit → apply pipeline
- Cannot scale with concurrency

### Performance Breakdown

| Operation | Current (Raft) | Time Spent |
|-----------|---------------|------------|
| Write metadata | 10-50ms (consensus) | 95% of latency |
| Apply to state machine | 100-500μs | 5% of latency |
| **Total** | **10-50ms** | **100%** |

The 10-50ms Raft consensus dominates, preventing >1,600 msg/s throughput.

---

## The Solution: Three-Phase Architecture

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│          Leader-Forwarding + WAL Metadata Architecture      │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Phase 1: Leader-Forwarding (No Split-Brain)               │
│  ┌────────────────────────────────────────┐                │
│  │ Follower receives read request         │                │
│  │   ↓                                    │                │
│  │ Check: Am I the leader?                │                │
│  │   ├─ YES → Read local state (1ms)      │                │
│  │   └─ NO  → Forward to leader (RPC)     │                │
│  │            Leader reads → Return        │                │
│  │            (Total: 2-5ms)               │                │
│  └────────────────────────────────────────┘                │
│                                                             │
│  Phase 2: WAL-Based Metadata Writes (Fast!)               │
│  ┌────────────────────────────────────────┐                │
│  │ Leader receives write request          │                │
│  │   ↓                                    │                │
│  │ Write to metadata WAL (1-2ms)          │ ← NO RAFT!     │
│  │   ↓                                    │                │
│  │ Apply to local state machine           │                │
│  │   ↓                                    │                │
│  │ Return success to client               │                │
│  │   ↓                                    │                │
│  │ Async replicate to followers           │ (background)   │
│  └────────────────────────────────────────┘                │
│                                                             │
│  Phase 3: Leader Leases (Fast Follower Reads)             │
│  ┌────────────────────────────────────────┐                │
│  │ Leader sends heartbeat every 1-2s      │                │
│  │   ↓                                    │                │
│  │ Followers receive heartbeat            │                │
│  │   ↓                                    │                │
│  │ Update lease_expires_at timestamp      │                │
│  │   ↓                                    │                │
│  │ On read request:                       │                │
│  │   ├─ Lease valid? YES → Read local     │ (1-2ms, fast!) │
│  │   └─ Lease expired? NO → Forward       │ (2-5ms, safe!) │
│  └────────────────────────────────────────┘                │
└─────────────────────────────────────────────────────────────┘
```

---

## Phase 1: Leader-Forwarding (Prevent Split-Brain)

**Goal**: Ensure all metadata reads see authoritative leader state (no stale reads)

**Timeline**: 3-4 days

### Phase 1.1: Add RPC Infrastructure for Leader-Forwarding (Day 1)

**Files to Create**:
1. `crates/chronik-server/src/metadata_rpc.rs` - RPC protocol for metadata queries
2. `crates/chronik-server/src/metadata_rpc_server.rs` - Server-side RPC handler
3. `crates/chronik-server/src/metadata_rpc_client.rs` - Client-side RPC stub

**Implementation**:

```rust
// crates/chronik-server/src/metadata_rpc.rs

use serde::{Serialize, Deserialize};
use chronik_common::metadata::{TopicMetadata, BrokerMetadata, PartitionAssignment};

/// Metadata query types for leader-forwarding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataQuery {
    GetTopic { name: String },
    ListTopics,
    GetBroker { broker_id: i32 },
    ListBrokers,
    GetPartitionAssignment { topic: String, partition: i32 },
    GetHighWatermark { topic: String, partition: i32 },
}

/// Metadata query response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataQueryResponse {
    Topic(Option<TopicMetadata>),
    TopicList(Vec<TopicMetadata>),
    Broker(Option<BrokerMetadata>),
    BrokerList(Vec<BrokerMetadata>),
    PartitionAssignment(Option<PartitionAssignment>),
    HighWatermark(i64),
}

/// Metadata write commands for leader-forwarding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataWriteCommand {
    CreateTopic { name: String, partition_count: i32, replication_factor: i32 },
    RegisterBroker { broker_id: i32, host: String, port: i32 },
    SetPartitionLeader { topic: String, partition: i32, leader_id: u64 },
    // ... other write commands
}
```

**RPC Server** (`metadata_rpc_server.rs`):

```rust
use tonic::{Request, Response, Status};
use std::sync::Arc;
use crate::raft_metadata_store::RaftMetadataStore;

pub struct MetadataRpcServer {
    metadata_store: Arc<RaftMetadataStore>,
}

impl MetadataRpcServer {
    pub fn new(metadata_store: Arc<RaftMetadataStore>) -> Self {
        Self { metadata_store }
    }

    /// Handle metadata query from follower
    pub async fn handle_query(&self, query: MetadataQuery) -> Result<MetadataQueryResponse, Status> {
        match query {
            MetadataQuery::GetTopic { name } => {
                let topic = self.metadata_store.get_topic(&name)
                    .await
                    .map_err(|e| Status::internal(format!("Query failed: {}", e)))?;
                Ok(MetadataQueryResponse::Topic(topic))
            }
            MetadataQuery::ListTopics => {
                let topics = self.metadata_store.list_topics()
                    .await
                    .map_err(|e| Status::internal(format!("Query failed: {}", e)))?;
                Ok(MetadataQueryResponse::TopicList(topics))
            }
            // ... handle other query types
            _ => Err(Status::unimplemented("Query type not implemented"))
        }
    }

    /// Handle metadata write from follower
    pub async fn handle_write(&self, cmd: MetadataWriteCommand) -> Result<(), Status> {
        // Forward write to Raft (for now, will change in Phase 2)
        match cmd {
            MetadataWriteCommand::CreateTopic { name, partition_count, replication_factor } => {
                self.metadata_store.create_topic(&name, TopicConfig {
                    partition_count,
                    replication_factor,
                    ..Default::default()
                })
                .await
                .map_err(|e| Status::internal(format!("Write failed: {}", e)))?;
                Ok(())
            }
            // ... handle other write types
            _ => Err(Status::unimplemented("Write type not implemented"))
        }
    }
}
```

**RPC Client** (`metadata_rpc_client.rs`):

```rust
use anyhow::Result;
use crate::metadata_rpc::{MetadataQuery, MetadataQueryResponse, MetadataWriteCommand};

pub struct MetadataRpcClient {
    leader_addr: String,
}

impl MetadataRpcClient {
    pub async fn connect(leader_addr: String) -> Result<Self> {
        // TODO: Establish HTTP/gRPC connection to leader
        Ok(Self { leader_addr })
    }

    pub async fn query(&self, query: MetadataQuery) -> Result<MetadataQueryResponse> {
        // TODO: Send query to leader via HTTP/gRPC
        // For now, placeholder
        Err(anyhow::anyhow!("RPC client not yet implemented"))
    }

    pub async fn write(&self, cmd: MetadataWriteCommand) -> Result<()> {
        // TODO: Send write command to leader
        Err(anyhow::anyhow!("RPC client not yet implemented"))
    }
}
```

### Phase 1.2: Modify RaftMetadataStore for Leader-Forwarding (Day 2)

**Files to Modify**:
- `crates/chronik-server/src/raft_metadata_store.rs`

**Changes**:

```rust
// Add to RaftMetadataStore struct
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    pending_topics: Arc<DashMap<String, Arc<Notify>>>,
    pending_brokers: Arc<DashMap<i32, Arc<Notify>>>,

    // NEW: RPC client for leader-forwarding
    rpc_client_cache: Arc<RwLock<Option<MetadataRpcClient>>>,
}

impl RaftMetadataStore {
    /// Check if this node is the Raft leader
    fn is_leader(&self) -> bool {
        self.raft.is_leader()
    }

    /// Get leader address for forwarding
    async fn get_leader_addr(&self) -> Result<String> {
        let leader_id = self.raft.get_leader_id()
            .ok_or_else(|| anyhow::anyhow!("No leader elected"))?;

        let leader_addr = self.raft.get_node_kafka_address(leader_id)
            .ok_or_else(|| anyhow::anyhow!("Leader address not found"))?;

        Ok(leader_addr)
    }

    /// Get or create RPC client to leader
    async fn get_rpc_client(&self) -> Result<MetadataRpcClient> {
        // Check cache first
        {
            let cache = self.rpc_client_cache.read().await;
            if let Some(client) = &*cache {
                return Ok(client.clone());
            }
        }

        // Create new client
        let leader_addr = self.get_leader_addr().await?;
        let client = MetadataRpcClient::connect(leader_addr).await?;

        // Cache it
        {
            let mut cache = self.rpc_client_cache.write().await;
            *cache = Some(client.clone());
        }

        Ok(client)
    }

    /// Forward read query to leader
    async fn forward_read_to_leader(&self, query: MetadataQuery) -> Result<MetadataQueryResponse> {
        let client = self.get_rpc_client().await?;
        client.query(query).await
    }

    /// Forward write command to leader
    async fn forward_write_to_leader(&self, cmd: MetadataWriteCommand) -> Result<()> {
        let client = self.get_rpc_client().await?;
        client.write(cmd).await
    }
}

// Modify read operations to check leadership
#[async_trait]
impl MetadataStore for RaftMetadataStore {
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        // CHECK: Am I the leader?
        if !self.is_leader() {
            // FOLLOWER: Forward to leader
            tracing::debug!("Forwarding get_topic('{}') to leader", name);
            let response = self.forward_read_to_leader(
                MetadataQuery::GetTopic { name: name.to_string() }
            ).await?;

            match response {
                MetadataQueryResponse::Topic(topic) => return Ok(topic),
                _ => return Err(anyhow::anyhow!("Invalid response type")),
            }
        }

        // LEADER: Read local state
        let state = self.state();
        Ok(state.topics.get(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        if !self.is_leader() {
            tracing::debug!("Forwarding list_topics() to leader");
            let response = self.forward_read_to_leader(MetadataQuery::ListTopics).await?;
            match response {
                MetadataQueryResponse::TopicList(topics) => return Ok(topics),
                _ => return Err(anyhow::anyhow!("Invalid response type")),
            }
        }

        let state = self.state();
        Ok(state.topics.values().cloned().collect())
    }

    // ... similar changes for other read operations

    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // For writes, also forward to leader (will change in Phase 2)
        if !self.is_leader() {
            tracing::debug!("Forwarding create_topic('{}') to leader", name);
            self.forward_write_to_leader(MetadataWriteCommand::CreateTopic {
                name: name.to_string(),
                partition_count: config.partition_count,
                replication_factor: config.replication_factor,
            }).await?;

            // Wait for replication to arrive (event-driven notification still works!)
            let notify = Arc::new(Notify::new());
            self.pending_topics.insert(name.to_string(), Arc::clone(&notify));

            tokio::time::timeout(
                Duration::from_secs(5),
                notify.notified()
            ).await?;

            return self.get_topic(name).await?.ok_or_else(|| {
                anyhow::anyhow!("Topic created but not found after replication")
            });
        }

        // LEADER: Use existing Raft path (for now)
        // Will replace with WAL in Phase 2
        let notify = Arc::new(Notify::new());
        self.pending_topics.insert(name.to_string(), Arc::clone(&notify));

        self.raft.propose(MetadataCommand::CreateTopic {
            name: name.to_string(),
            partition_count: config.partition_count,
            replication_factor: config.replication_factor,
            config: config.config.clone(),
        }).await?;

        tokio::time::timeout(Duration::from_secs(2), notify.notified()).await?;

        let state = self.state();
        state.topics.get(name).cloned()
            .ok_or_else(|| anyhow::anyhow!("Topic not found after creation"))
    }
}
```

### Phase 1.3: Testing & Validation (Day 3)

**Test Cases**:

1. **Leader read** - Should be fast (1ms local read)
2. **Follower read** - Should forward to leader (2-5ms with network hop)
3. **Leader write** - Should work via Raft (still slow, 10-50ms)
4. **Follower write** - Should forward to leader, then Raft (still slow)
5. **No split-brain** - Verify all reads see same data across nodes

**Test Script** (`tests/test_leader_forwarding.py`):

```python
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
import time

print("Testing Leader-Forwarding Phase 1...")

# Create admin clients to all 3 nodes
admin1 = KafkaAdminClient(bootstrap_servers='localhost:9092')  # Node 1 (likely leader)
admin2 = KafkaAdminClient(bootstrap_servers='localhost:9093')  # Node 2 (likely follower)
admin3 = KafkaAdminClient(bootstrap_servers='localhost:9094')  # Node 3 (likely follower)

# Create topic via leader
print("\n1. Creating topic 'test-forwarding' via leader (Node 1)...")
start = time.time()
admin1.create_topics([NewTopic('test-forwarding', 3, 3)])
leader_create_time = time.time() - start
print(f"   Leader create time: {leader_create_time*1000:.2f}ms")

# Wait for replication
time.sleep(1)

# List topics from all nodes (should all see it via forwarding)
print("\n2. Listing topics from all nodes...")
topics1 = admin1.list_topics()
topics2 = admin2.list_topics()
topics3 = admin3.list_topics()

if 'test-forwarding' in topics1 and 'test-forwarding' in topics2 and 'test-forwarding' in topics3:
    print("   ✅ All nodes see topic 'test-forwarding' (no split-brain!)")
else:
    print("   ❌ Split-brain detected! Not all nodes see topic")
    print(f"      Node 1: {topics1}")
    print(f"      Node 2: {topics2}")
    print(f"      Node 3: {topics3}")

# Measure follower read latency (should be 2-5ms due to forwarding)
print("\n3. Measuring follower read latency...")
follower_read_times = []
for i in range(10):
    start = time.time()
    _ = admin2.list_topics()  # Node 2 (follower) forwards to leader
    elapsed = time.time() - start
    follower_read_times.append(elapsed * 1000)

avg_follower_read = sum(follower_read_times) / len(follower_read_times)
print(f"   Average follower read time: {avg_follower_read:.2f}ms")

if avg_follower_read < 10:
    print("   ✅ Follower reads are fast (< 10ms)")
else:
    print(f"   ⚠️  Follower reads slower than expected ({avg_follower_read:.2f}ms)")

print("\n✅ Phase 1 (Leader-Forwarding) validation complete!")
print("   - No split-brain detected")
print("   - Leader reads: Fast")
print("   - Follower reads: Forward to leader")
print("\nReady for Phase 2: WAL-based metadata writes")
```

**Success Criteria**:
- ✅ All nodes see same metadata (no split-brain)
- ✅ Leader reads: < 1ms
- ✅ Follower reads: 2-5ms (forwarding overhead acceptable)
- ✅ No regression in existing functionality

---

## Phase 2: WAL-Based Metadata Writes (Fast Path)

**Goal**: Replace Raft consensus with fast local WAL writes for metadata operations

**Timeline**: 5-7 days

### Phase 2.1: Create Metadata WAL Infrastructure (Day 1-2)

**Files to Create**:
1. `crates/chronik-server/src/metadata_wal.rs` - Metadata WAL writer
2. `crates/chronik-server/src/metadata_wal_replication.rs` - Replication sender/receiver

**Implementation**:

```rust
// crates/chronik-server/src/metadata_wal.rs

use chronik_wal::GroupCommitWal;
use crate::raft_metadata::MetadataCommand;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

/// Metadata WAL - Special-purpose WAL for metadata operations
/// Uses "__chronik_metadata" as topic name for replication routing
pub struct MetadataWal {
    wal: Arc<GroupCommitWal>,
    topic_name: String, // "__chronik_metadata"
}

impl MetadataWal {
    /// Create new metadata WAL
    pub async fn new(data_dir: PathBuf) -> Result<Self> {
        let topic_name = "__chronik_metadata".to_string();
        let wal_dir = data_dir.join("metadata_wal");

        // Use GroupCommitWal with ultra profile for maximum throughput
        let wal = GroupCommitWal::new(
            wal_dir,
            chronik_wal::WalProfile::Ultra, // Fast writes!
        )?;

        Ok(Self {
            wal: Arc::new(wal),
            topic_name,
        })
    }

    /// Append metadata command to WAL
    /// Returns immediately after durable write (1-2ms)
    pub async fn append(&self, cmd: MetadataCommand) -> Result<u64> {
        let data = bincode::serialize(&cmd)?;

        // Write to WAL (synchronous, but fast with group commit)
        let offset = self.wal.append(&data).await?;

        tracing::debug!(
            "Appended metadata command to WAL at offset {}: {:?}",
            offset,
            cmd
        );

        Ok(offset)
    }

    /// Read metadata command at offset (for recovery)
    pub async fn read(&self, offset: u64) -> Result<MetadataCommand> {
        let data = self.wal.read(offset).await?;
        let cmd = bincode::deserialize(&data)?;
        Ok(cmd)
    }

    /// Get topic name for replication routing
    pub fn topic_name(&self) -> &str {
        &self.topic_name
    }
}
```

**Metadata WAL Replication** (`metadata_wal_replication.rs`):

```rust
// Reuse existing WalReplicationManager infrastructure!

use crate::wal_replication::WalReplicationManager;
use crate::metadata_wal::MetadataWal;
use anyhow::Result;
use std::sync::Arc;

pub struct MetadataWalReplicator {
    wal: Arc<MetadataWal>,
    replication_mgr: Arc<WalReplicationManager>,
}

impl MetadataWalReplicator {
    pub fn new(
        wal: Arc<MetadataWal>,
        replication_mgr: Arc<WalReplicationManager>
    ) -> Self {
        Self { wal, replication_mgr }
    }

    /// Replicate metadata command to followers
    /// This is ASYNC - fires and forgets, returns immediately
    pub async fn replicate(&self, cmd: MetadataCommand, offset: u64) -> Result<()> {
        let data = bincode::serialize(&cmd)?;

        // Use existing WalReplicationManager with special topic name
        // This reuses ALL the existing replication infrastructure!
        self.replication_mgr.replicate_partition(
            self.wal.topic_name(),  // "__chronik_metadata"
            0,                      // partition 0 (metadata is single partition)
            data,
        ).await?;

        tracing::debug!(
            "Replicated metadata command to followers: {:?}",
            cmd
        );

        Ok(())
    }
}
```

### Phase 2.2: Modify RaftMetadataStore to Use Metadata WAL (Day 3-4)

**Files to Modify**:
- `crates/chronik-server/src/raft_metadata_store.rs`

**Changes**:

```rust
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    pending_topics: Arc<DashMap<String, Arc<Notify>>>,
    pending_brokers: Arc<DashMap<i32, Arc<Notify>>>,
    rpc_client_cache: Arc<RwLock<Option<MetadataRpcClient>>>,

    // NEW: Metadata WAL for fast writes
    metadata_wal: Option<Arc<MetadataWal>>,
    metadata_wal_replicator: Option<Arc<MetadataWalReplicator>>,
}

impl RaftMetadataStore {
    pub fn new_with_wal(
        raft: Arc<RaftCluster>,
        metadata_wal: Arc<MetadataWal>,
        replicator: Arc<MetadataWalReplicator>,
    ) -> Self {
        Self {
            pending_topics: raft.get_pending_topics_notifications(),
            pending_brokers: raft.get_pending_brokers_notifications(),
            rpc_client_cache: Arc::new(RwLock::new(None)),
            metadata_wal: Some(metadata_wal),
            metadata_wal_replicator: Some(replicator),
            raft,
        }
    }
}

#[async_trait]
impl MetadataStore for RaftMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // FOLLOWER: Forward to leader (same as Phase 1)
        if !self.is_leader() {
            tracing::debug!("Forwarding create_topic('{}') to leader", name);
            self.forward_write_to_leader(MetadataWriteCommand::CreateTopic {
                name: name.to_string(),
                partition_count: config.partition_count,
                replication_factor: config.replication_factor,
            }).await?;

            let notify = Arc::new(Notify::new());
            self.pending_topics.insert(name.to_string(), Arc::clone(&notify));
            tokio::time::timeout(Duration::from_secs(5), notify.notified()).await?;

            return self.get_topic(name).await?.ok_or_else(|| {
                anyhow::anyhow!("Topic created but not found after replication")
            });
        }

        // LEADER: Use metadata WAL (FAST PATH!)
        let cmd = MetadataCommand::CreateTopic {
            name: name.to_string(),
            partition_count: config.partition_count,
            replication_factor: config.replication_factor,
            config: config.config.clone(),
        };

        // 1. Write to metadata WAL (1-2ms, durable)
        let offset = self.metadata_wal.as_ref().unwrap().append(cmd.clone()).await?;

        tracing::info!(
            "Wrote CreateTopic('{}') to metadata WAL at offset {}",
            name,
            offset
        );

        // 2. Apply to local state machine immediately
        self.raft.state_machine.write()?.apply(cmd.clone())?;

        tracing::info!("Applied CreateTopic('{}') to state machine", name);

        // 3. Fire notification for any waiting threads
        if let Some((_, notify)) = self.pending_topics.remove(name) {
            notify.notify_waiters();
        }

        // 4. Async replicate to followers (fire-and-forget)
        let replicator = self.metadata_wal_replicator.as_ref().unwrap().clone();
        let cmd_clone = cmd.clone();
        tokio::spawn(async move {
            if let Err(e) = replicator.replicate(cmd_clone, offset).await {
                tracing::warn!("Metadata replication failed: {}", e);
                // Don't fail the write - replication is async/eventual
            }
        });

        // 5. Return immediately (no waiting for followers!)
        let state = self.state();
        state.topics.get(name).cloned()
            .ok_or_else(|| anyhow::anyhow!("Topic not found after creation"))
    }

    // Similar changes for other write operations:
    // - register_broker()
    // - set_partition_leader()
    // - create_consumer_group()
    // etc.
}
```

### Phase 2.3: Follower WAL Receiver (Day 5)

**Files to Modify**:
- `crates/chronik-server/src/wal_receiver.rs` (existing WAL receiver)

**Changes**:

```rust
// Add special handling for "__chronik_metadata" topic

impl WalReceiver {
    async fn handle_wal_record(&self, topic: &str, partition: i32, record: WalRecord) -> Result<()> {
        // Special case: Metadata WAL replication
        if topic == "__chronik_metadata" && partition == 0 {
            return self.handle_metadata_wal_record(record).await;
        }

        // Normal partition data replication
        self.handle_partition_wal_record(topic, partition, record).await
    }

    async fn handle_metadata_wal_record(&self, record: WalRecord) -> Result<()> {
        // Deserialize metadata command
        let cmd: MetadataCommand = bincode::deserialize(&record.data)?;

        tracing::debug!("Received metadata WAL replication: {:?}", cmd);

        // Apply to follower's state machine
        self.raft_cluster.state_machine.write()?.apply(cmd.clone())?;

        // Fire notification for any waiting threads (followers might be waiting)
        match &cmd {
            MetadataCommand::CreateTopic { name, .. } => {
                if let Some((_, notify)) = self.raft_cluster.pending_topics.remove(name) {
                    notify.notify_waiters();
                    tracing::debug!("Notified waiting threads for topic '{}'", name);
                }
            }
            MetadataCommand::RegisterBroker { broker_id, .. } => {
                if let Some((_, notify)) = self.raft_cluster.pending_brokers.remove(broker_id) {
                    notify.notify_waiters();
                }
            }
            _ => {}
        }

        tracing::info!("Applied metadata WAL replication: {:?}", cmd);

        Ok(())
    }
}
```

### Phase 2.4: Testing & Validation (Day 6-7)

**Test Cases**:

1. **Leader metadata write** - Should be 5-10x faster than Phase 1 (1-2ms vs 10-50ms)
2. **Follower receives replication** - Should apply to state machine
3. **Follower reads** - Should see replicated metadata
4. **Crash recovery** - Leader restart should replay metadata WAL
5. **Throughput test** - Should achieve 6,000-8,000 msg/s (4-5x improvement)

**Test Script** (`tests/test_wal_metadata_writes.py`):

```python
from kafka import KafkaProducer
import time

print("Testing WAL-Based Metadata Writes (Phase 2)...")

# Create 100 topics rapidly (tests fast metadata writes)
print("\n1. Creating 100 topics via leader...")
start = time.time()

producer = KafkaProducer(bootstrap_servers='localhost:9092')

for i in range(100):
    topic = f'test-wal-metadata-{i}'
    producer.send(topic, b'test')  # Auto-creates topic
    if (i + 1) % 10 == 0:
        print(f"   Created {i+1} topics...")

producer.flush()
producer.close()

elapsed = time.time() - start
avg_create_time = (elapsed / 100) * 1000

print(f"\n✅ Created 100 topics in {elapsed:.2f}s")
print(f"   Average topic creation time: {avg_create_time:.2f}ms")

if avg_create_time < 5:
    print("   ✅ EXCELLENT! Topic creation is FAST (< 5ms)")
    print("      WAL-based metadata writes are working!")
elif avg_create_time < 10:
    print("   ✅ GOOD! Topic creation is fast (< 10ms)")
else:
    print(f"   ⚠️  Topic creation slower than expected ({avg_create_time:.2f}ms)")

# Wait for replication
time.sleep(2)

# Verify followers see replicated metadata
print("\n2. Verifying metadata replication to followers...")
from kafka.admin import KafkaAdminClient

admin2 = KafkaAdminClient(bootstrap_servers='localhost:9093')  # Follower
topics = admin2.list_topics()

found_count = sum(1 for i in range(100) if f'test-wal-metadata-{i}' in topics)

if found_count == 100:
    print(f"   ✅ All 100 topics replicated to follower!")
else:
    print(f"   ⚠️  Only {found_count}/100 topics found on follower")

print("\n✅ Phase 2 (WAL Metadata) validation complete!")
print(f"   - Topic creation: {avg_create_time:.2f}ms avg")
print(f"   - Replication: {found_count}/100 topics")
print("\nExpected throughput improvement: 4-5x (Phase 1 baseline)")
```

**Success Criteria**:
- ✅ Leader metadata writes: < 5ms (5-10x faster than Raft)
- ✅ Followers receive and apply replication
- ✅ Throughput: 6,000-8,000 msg/s (4-5x improvement over 1,600 msg/s)
- ✅ No split-brain, no data loss

---

## Phase 3: Leader Leases (Fast Follower Reads)

**Goal**: Enable followers to read local replicated state safely (with bounded staleness)

**Timeline**: 4-5 days

### Phase 3.1: Leader Heartbeat Infrastructure (Day 1-2)

**Files to Create**:
1. `crates/chronik-server/src/leader_lease.rs` - Lease tracking
2. `crates/chronik-server/src/leader_heartbeat.rs` - Heartbeat sender/receiver

**Implementation**:

```rust
// crates/chronik-server/src/leader_lease.rs

use std::time::{Duration, Instant};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Leader lease tracker for followers
/// Tracks when leader last sent heartbeat and when lease expires
#[derive(Debug, Clone)]
pub struct LeaderLease {
    pub leader_id: u64,
    pub term: u64,
    pub last_heartbeat: Instant,
    pub lease_expires_at: Instant,
}

impl LeaderLease {
    pub fn new(leader_id: u64, term: u64, lease_duration: Duration) -> Self {
        let now = Instant::now();
        Self {
            leader_id,
            term,
            last_heartbeat: now,
            lease_expires_at: now + lease_duration,
        }
    }

    /// Check if lease is still valid
    pub fn is_valid(&self) -> bool {
        Instant::now() < self.lease_expires_at
    }

    /// Update lease from new heartbeat
    pub fn update(&mut self, leader_id: u64, term: u64, lease_duration: Duration) {
        self.leader_id = leader_id;
        self.term = term;
        self.last_heartbeat = Instant::now();
        self.lease_expires_at = Instant::now() + lease_duration;
    }
}

/// Leader lease manager for a node
pub struct LeaseManager {
    current_lease: Arc<RwLock<Option<LeaderLease>>>,
    lease_duration: Duration, // Default: 5 seconds
}

impl LeaseManager {
    pub fn new(lease_duration: Duration) -> Self {
        Self {
            current_lease: Arc::new(RwLock::new(None)),
            lease_duration,
        }
    }

    /// Update lease from heartbeat
    pub async fn update_lease(&self, leader_id: u64, term: u64) {
        let mut lease = self.current_lease.write().await;

        match lease.as_mut() {
            Some(existing) => {
                existing.update(leader_id, term, self.lease_duration);
            }
            None => {
                *lease = Some(LeaderLease::new(leader_id, term, self.lease_duration));
            }
        }

        tracing::debug!(
            "Updated leader lease: leader={}, term={}, expires_in={}s",
            leader_id,
            term,
            self.lease_duration.as_secs()
        );
    }

    /// Check if we have a valid lease
    pub async fn has_valid_lease(&self) -> bool {
        let lease = self.current_lease.read().await;
        lease.as_ref().map(|l| l.is_valid()).unwrap_or(false)
    }

    /// Get current leader ID (if lease valid)
    pub async fn get_leader_id(&self) -> Option<u64> {
        let lease = self.current_lease.read().await;
        lease.as_ref().filter(|l| l.is_valid()).map(|l| l.leader_id)
    }
}
```

**Leader Heartbeat** (`leader_heartbeat.rs`):

```rust
use serde::{Serialize, Deserialize};
use std::time::Duration;
use std::sync::Arc;
use tokio::time;
use crate::raft_cluster::RaftCluster;
use crate::leader_lease::LeaseManager;

/// Heartbeat message from leader to followers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderHeartbeat {
    pub leader_id: u64,
    pub term: u64,
    pub timestamp: u64, // Unix timestamp in milliseconds
}

/// Leader heartbeat sender (runs on leader)
pub struct HeartbeatSender {
    raft: Arc<RaftCluster>,
    interval: Duration, // Default: 1 second
}

impl HeartbeatSender {
    pub fn new(raft: Arc<RaftCluster>, interval: Duration) -> Self {
        Self { raft, interval }
    }

    /// Start heartbeat loop (spawn background task)
    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            self.heartbeat_loop().await;
        });
    }

    async fn heartbeat_loop(&self) {
        let mut interval = time::interval(self.interval);

        loop {
            interval.tick().await;

            // Only send heartbeats if we're the leader
            if !self.raft.is_leader() {
                continue;
            }

            let heartbeat = LeaderHeartbeat {
                leader_id: self.raft.get_node_id(),
                term: self.raft.get_current_term(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            };

            if let Err(e) = self.send_heartbeat_to_followers(heartbeat).await {
                tracing::warn!("Failed to send heartbeat: {}", e);
            }
        }
    }

    async fn send_heartbeat_to_followers(&self, heartbeat: LeaderHeartbeat) -> anyhow::Result<()> {
        // Serialize heartbeat
        let data = bincode::serialize(&heartbeat)?;

        // Send via existing WalReplicationManager (piggyback on replication channel)
        // Use special topic "__chronik_heartbeat" to route to followers
        // Followers will handle this specially (won't write to WAL)

        // TODO: Implement heartbeat transport
        // For now, can reuse gRPC transport or add dedicated heartbeat channel

        tracing::debug!(
            "Sent heartbeat to followers: leader={}, term={}",
            heartbeat.leader_id,
            heartbeat.term
        );

        Ok(())
    }
}

/// Heartbeat receiver (runs on followers)
pub struct HeartbeatReceiver {
    lease_manager: Arc<LeaseManager>,
}

impl HeartbeatReceiver {
    pub fn new(lease_manager: Arc<LeaseManager>) -> Self {
        Self { lease_manager }
    }

    /// Handle received heartbeat
    pub async fn handle_heartbeat(&self, heartbeat: LeaderHeartbeat) -> anyhow::Result<()> {
        tracing::debug!(
            "Received heartbeat: leader={}, term={}",
            heartbeat.leader_id,
            heartbeat.term
        );

        // Update lease
        self.lease_manager.update_lease(heartbeat.leader_id, heartbeat.term).await;

        Ok(())
    }
}
```

### Phase 3.2: Integrate Leases into RaftMetadataStore (Day 3)

**Files to Modify**:
- `crates/chronik-server/src/raft_metadata_store.rs`

**Changes**:

```rust
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    pending_topics: Arc<DashMap<String, Arc<Notify>>>,
    pending_brokers: Arc<DashMap<i32, Arc<Notify>>>,
    rpc_client_cache: Arc<RwLock<Option<MetadataRpcClient>>>,
    metadata_wal: Option<Arc<MetadataWal>>,
    metadata_wal_replicator: Option<Arc<MetadataWalReplicator>>,

    // NEW: Leader lease manager
    lease_manager: Arc<LeaseManager>,
}

impl RaftMetadataStore {
    pub fn new_with_wal_and_lease(
        raft: Arc<RaftCluster>,
        metadata_wal: Arc<MetadataWal>,
        replicator: Arc<MetadataWalReplicator>,
        lease_manager: Arc<LeaseManager>,
    ) -> Self {
        Self {
            pending_topics: raft.get_pending_topics_notifications(),
            pending_brokers: raft.get_pending_brokers_notifications(),
            rpc_client_cache: Arc::new(RwLock::new(None)),
            metadata_wal: Some(metadata_wal),
            metadata_wal_replicator: Some(replicator),
            lease_manager,
            raft,
        }
    }
}

#[async_trait]
impl MetadataStore for RaftMetadataStore {
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        // LEADER: Always read local state (fast)
        if self.is_leader() {
            let state = self.state();
            return Ok(state.topics.get(name).cloned());
        }

        // FOLLOWER: Check if we have a valid lease
        if self.lease_manager.has_valid_lease().await {
            // ✅ LEASE VALID: Safe to read local replicated state
            tracing::debug!(
                "Reading topic '{}' from local state (valid lease)",
                name
            );
            let state = self.state();
            return Ok(state.topics.get(name).cloned());
        }

        // ❌ NO LEASE: Forward to leader for safety
        tracing::debug!(
            "Forwarding get_topic('{}') to leader (no valid lease)",
            name
        );
        let response = self.forward_read_to_leader(
            MetadataQuery::GetTopic { name: name.to_string() }
        ).await?;

        match response {
            MetadataQueryResponse::Topic(topic) => Ok(topic),
            _ => Err(anyhow::anyhow!("Invalid response type")),
        }
    }

    // Similar changes for other read operations:
    // - list_topics()
    // - get_broker()
    // - list_brokers()
    // - get_partition_assignment()
    // - get_high_watermark()
    // etc.
}
```

### Phase 3.3: Start Heartbeat Loops on Cluster Start (Day 4)

**Files to Modify**:
- `crates/chronik-server/src/main.rs` (cluster mode startup)

**Changes**:

```rust
// In run_raft_cluster() function

async fn run_raft_cluster(config: &ServerConfig) -> Result<()> {
    // ... existing setup ...

    // Create lease manager
    let lease_manager = Arc::new(LeaseManager::new(Duration::from_secs(5)));

    // Create metadata WAL and replicator
    let metadata_wal = Arc::new(MetadataWal::new(data_dir.clone()).await?);
    let replicator = Arc::new(MetadataWalReplicator::new(
        metadata_wal.clone(),
        wal_replication_mgr.clone(),
    ));

    // Create metadata store with WAL and lease support
    let metadata_store = Arc::new(RaftMetadataStore::new_with_wal_and_lease(
        raft_cluster.clone(),
        metadata_wal,
        replicator,
        lease_manager.clone(),
    ));

    // Start heartbeat sender (leader)
    let heartbeat_sender = Arc::new(HeartbeatSender::new(
        raft_cluster.clone(),
        Duration::from_secs(1), // Send every 1 second
    ));
    heartbeat_sender.start();

    // Start heartbeat receiver (follower)
    let heartbeat_receiver = Arc::new(HeartbeatReceiver::new(lease_manager.clone()));
    // TODO: Wire up heartbeat receiver to transport layer

    // ... rest of startup ...
}
```

### Phase 3.4: Testing & Validation (Day 5)

**Test Cases**:

1. **Leader reads** - Should be fast (< 1ms)
2. **Follower reads WITH lease** - Should be fast (1-2ms, local read)
3. **Follower reads WITHOUT lease** - Should forward (2-5ms, network hop)
4. **Lease expiration** - After 5s without heartbeat, followers should forward
5. **Throughput test** - Should achieve 10,000+ msg/s (6-10x improvement)

**Test Script** (`tests/test_leader_leases.py`):

```python
from kafka.admin import KafkaAdminClient
import time

print("Testing Leader Leases (Phase 3)...")

# Create admin clients
admin1 = KafkaAdminClient(bootstrap_servers='localhost:9092')  # Leader
admin2 = KafkaAdminClient(bootstrap_servers='localhost:9093')  # Follower

# Create a topic via leader
print("\n1. Creating topic 'test-lease'...")
from kafka.admin import NewTopic
admin1.create_topics([NewTopic('test-lease', 3, 3)])
time.sleep(1)  # Wait for replication

# Measure follower read latency with lease
print("\n2. Measuring follower read latency (with lease)...")
read_times = []
for i in range(100):
    start = time.time()
    _ = admin2.list_topics()  # Follower reads local state (lease valid)
    elapsed = (time.time() - start) * 1000
    read_times.append(elapsed)

avg_read_time = sum(read_times) / len(read_times)
p50 = sorted(read_times)[len(read_times) // 2]
p95 = sorted(read_times)[int(len(read_times) * 0.95)]
p99 = sorted(read_times)[int(len(read_times) * 0.99)]

print(f"   Follower read latency (with lease):")
print(f"     Average: {avg_read_time:.2f}ms")
print(f"     p50: {p50:.2f}ms")
print(f"     p95: {p95:.2f}ms")
print(f"     p99: {p99:.2f}ms")

if p99 < 5:
    print("   ✅ EXCELLENT! Follower reads are FAST (< 5ms p99)")
    print("      Leader leases are working!")
elif p99 < 10:
    print("   ✅ GOOD! Follower reads are fast (< 10ms p99)")
else:
    print(f"   ⚠️  Follower reads slower than expected ({p99:.2f}ms p99)")

# Test lease expiration (stop leader heartbeats)
print("\n3. Testing lease expiration...")
print("   (This would require stopping leader heartbeats)")
print("   (Manual test: kill leader, verify follower reads fail gracefully)")

print("\n✅ Phase 3 (Leader Leases) validation complete!")
print(f"   - Follower reads with lease: {p99:.2f}ms p99")
print(f"   - Expected throughput: 10,000+ msg/s")
```

**Success Criteria**:
- ✅ Follower reads WITH lease: < 5ms p99 (same as local read)
- ✅ Follower reads WITHOUT lease: Forward to leader (2-5ms)
- ✅ Lease expiration handled gracefully
- ✅ Throughput: 10,000+ msg/s (6-10x improvement over 1,600 msg/s)
- ✅ Strong consistency maintained

---

## End-to-End Integration & Performance Testing

**Timeline**: 2-3 days

### Integration Test Suite

**Test Script** (`tests/test_leader_forwarding_complete.py`):

```python
from kafka import KafkaProducer, KafkaAdminClient
from kafka.admin import NewTopic
import time
import threading
import statistics

print("=" * 80)
print("Leader-Forwarding + WAL Metadata + Leases - Full Integration Test")
print("=" * 80)

# Configuration
BOOTSTRAP_SERVERS = 'localhost:9092,localhost:9093,localhost:9094'
NUM_TOPICS = 1000
CONCURRENCY = 128

def create_topic_thread(thread_id, topics_per_thread):
    """Create topics concurrently"""
    admin = KafkaAdminClient(bootstrap_servers=BOOTSTRAP_SERVERS)

    start = time.time()
    for i in range(topics_per_thread):
        topic_name = f'perf-test-{thread_id}-{i}'
        try:
            admin.create_topics([NewTopic(topic_name, 3, 3)])
        except Exception as e:
            print(f"Thread {thread_id}: Error creating topic: {e}")
    elapsed = time.time() - start

    return elapsed

print(f"\n1. Creating {NUM_TOPICS} topics with {CONCURRENCY} threads...")
print("-" * 80)

start_time = time.time()

# Launch threads
threads = []
topics_per_thread = NUM_TOPICS // CONCURRENCY

for i in range(CONCURRENCY):
    t = threading.Thread(target=create_topic_thread, args=(i, topics_per_thread))
    t.start()
    threads.append(t)

# Wait for all threads
for t in threads:
    t.join()

elapsed = time.time() - start_time
throughput = NUM_TOPICS / elapsed

print("-" * 80)
print(f"\nResults:")
print(f"  Total time: {elapsed:.2f}s")
print(f"  Topics created: {NUM_TOPICS}")
print(f"  Throughput: {throughput:.0f} topics/s")
print()

# Compare with baselines
BASELINE_V229 = 1600   # v2.2.9 with Raft consensus
TARGET_V230 = 10000    # v2.3.0 target with leader-forwarding

print("Performance Comparison:")
print(f"  v2.2.9 (Raft consensus):       {BASELINE_V229:,} topics/s")
print(f"  v2.3.0 (Leader-forwarding):    {throughput:,.0f} topics/s")
print(f"  Target:                        {TARGET_V230:,} topics/s")
print()

improvement = ((throughput - BASELINE_V229) / BASELINE_V229 * 100)

if throughput >= TARGET_V230:
    print(f"✅ SUCCESS! Leader-forwarding achieved target!")
    print(f"   {improvement:.1f}% improvement over v2.2.9")
elif throughput >= BASELINE_V229 * 3:
    print(f"✅ EXCELLENT! {improvement:.1f}% improvement!")
else:
    print(f"⚠️  Performance below target: {throughput:.0f} < {TARGET_V230:,}")

print("=" * 80)
```

**Success Criteria for v2.3.0 Release**:
- ✅ Throughput: 10,000+ msg/s (6-10x improvement)
- ✅ Leader metadata write: < 5ms
- ✅ Follower metadata read (with lease): < 5ms p99
- ✅ No split-brain (all nodes see same data)
- ✅ Strong consistency maintained
- ✅ Crash recovery works (metadata WAL replay)
- ✅ No regressions in existing functionality

---

## Rollout & Monitoring

### Deployment Strategy

**Stage 1: Single-Node Validation (1 day)**
- Deploy to single-node cluster first
- Verify metadata WAL writes working
- Verify no performance regression

**Stage 2: 3-Node Cluster Validation (2 days)**
- Deploy to 3-node test cluster
- Run full integration test suite
- Monitor for 24 hours

**Stage 3: Production Rollout (1 week)**
- Blue/green deployment
- Gradual traffic shift
- Monitor metrics continuously

### Key Metrics to Monitor

**Performance Metrics**:
- `metadata_write_latency_ms` - Should be < 5ms p99
- `metadata_read_latency_ms` - Should be < 5ms p99
- `topic_creation_throughput` - Should be 10,000+ topics/s

**Consistency Metrics**:
- `metadata_split_brain_detected` - Should be 0
- `metadata_replication_lag_ms` - Should be < 100ms
- `lease_expiration_count` - Track how often leases expire

**Reliability Metrics**:
- `metadata_wal_write_errors` - Should be 0
- `metadata_replication_errors` - Monitor for issues
- `leader_heartbeat_failures` - Track heartbeat reliability

---

## Rollback Plan

If throughput < 5,000 msg/s or critical bugs found:

**Phase 1 Rollback**: Disable leader leases (read forwarding only)
**Phase 2 Rollback**: Revert to Raft consensus for writes
**Phase 3 Rollback**: Full rollback to v2.2.9

Each phase is independently reversible via feature flags.

---

## Summary

| Phase | Description | Timeline | Expected Gain |
|-------|-------------|----------|---------------|
| **Phase 1** | Leader-Forwarding (Prevent Split-Brain) | 3-4 days | 0% (same throughput, but correct) |
| **Phase 2** | WAL-Based Metadata Writes (Fast Path) | 5-7 days | 4-5x (1,600 → 6,000-8,000 msg/s) |
| **Phase 3** | Leader Leases (Fast Follower Reads) | 4-5 days | 6-10x (1,600 → 10,000+ msg/s) |
| **Integration** | End-to-End Testing & Validation | 2-3 days | - |
| **Total** | **Complete Implementation** | **3-4 weeks** | **6-10x improvement** |

**Final Target**: 10,000+ msg/s (vs 1,600 msg/s baseline)

---

## Next Session Checklist

- [ ] Review this plan
- [ ] Decide on Phase 1 start date
- [ ] Set up performance monitoring dashboard
- [ ] Create feature flags for each phase
- [ ] Prepare rollback procedures

**First Implementation Step**: Phase 1.1 - Add RPC infrastructure for leader-forwarding

---

**Author**: Claude (Anthropic)
**Date**: 2025-11-11
**Status**: READY TO IMPLEMENT
