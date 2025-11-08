# Metadata Split-Brain Fix Plan

**Date:** 2025-11-08
**Target Version:** v2.3.0 (next major release)
**Issue:** Dual metadata stores causing split-brain cluster state
**Prerequisites:** Read `docs/METADATA_SPLIT_BRAIN_ROOT_CAUSE.md`

## Overview

This plan consolidates Chronik's dual metadata architecture into a **single source of truth**: the Raft state machine for cluster mode, ChronikMetaLogStore for standalone mode.

## Goals

1. **Eliminate split-brain:** All nodes see identical metadata
2. **Raft-coordinated writes:** All metadata changes via Raft consensus
3. **Strong consistency:** Linearizable reads and writes
4. **Backward compatible:** Standalone mode unchanged
5. **Production-ready:** Comprehensive testing, migration path

## Proposed Architecture

### Cluster Mode (NEW)

```
┌──────────────────────────────────────────────────────────────┐
│             Unified Metadata Architecture (v2.3.0)           │
│                                                              │
│  Node 1                Node 2                Node 3         │
│  ┌─────────┐          ┌─────────┐          ┌─────────┐     │
│  │ Raft    │←────────→│ Raft    │←────────→│ Raft    │     │
│  │ State   │ Consensus │ State   │ Consensus │ State   │     │
│  │ Machine │          │ Machine │          │ Machine │     │
│  │         │          │         │          │         │     │
│  │ Topics  │          │ Topics  │          │ Topics  │     │
│  │ Offsets │          │ Offsets │          │ Offsets │     │
│  │ Groups  │          │ Groups  │          │ Groups  │     │
│  │ Brokers │          │ Brokers │          │ Brokers │     │
│  │ Part    │          │ Part    │          │ Part    │     │
│  │ Assigns │          │ Assigns │          │ Assigns │     │
│  │ Leaders │          │ Leaders │          │ Leaders │     │
│  │ ISR     │          │ ISR     │          │ ISR     │     │
│  └─────────┘          └─────────┘          └─────────┘     │
│       ↓                    ↓                    ↓           │
│  [Consistent]         [Consistent]         [Consistent]    │
│       ↓                    ↓                    ↓           │
│  ┌─────────┐          ┌─────────┐          ┌─────────┐     │
│  │ Raft    │←─Raft───→│ Raft    │←─Raft───→│ Raft    │     │
│  │ Meta    │   Log    │ Meta    │   Log    │ Meta    │     │
│  │ Store   │Replication│ Store   │Replication│ Store   │     │
│  │(Wrapper)│          │(Wrapper)│          │(Wrapper)│     │
│  └─────────┘          └─────────┘          └─────────┘     │
│       ↑                    ↑                    ↑           │
│  [MetadataStore trait - all queries/writes via Raft]       │
│                                                              │
│  SINGLE SOURCE OF TRUTH: Raft State Machine                │
└──────────────────────────────────────────────────────────────┘
```

### Standalone Mode (UNCHANGED)

```
┌──────────────────────────────────────────────────────┐
│        Standalone Mode (No Changes)                  │
│                                                      │
│  ┌─────────────────────┐                            │
│  │ ChronikMetaLogStore │                            │
│  │  (WAL-backed)       │                            │
│  │                     │                            │
│  │ Topics, Offsets,    │                            │
│  │ Groups, Brokers,    │                            │
│  │ Partitions          │                            │
│  └─────────────────────┘                            │
│           ↓                                          │
│  ┌─────────────────────┐                            │
│  │ Local __meta WAL    │                            │
│  └─────────────────────┘                            │
│                                                      │
│  Fast, simple, no Raft overhead                     │
└──────────────────────────────────────────────────────┘
```

## Implementation Phases

### Phase 1: Expand Raft State Machine (Foundation)

**Objective:** Add ALL metadata to MetadataStateMachine

**Files to modify:**
- `crates/chronik-server/src/raft_metadata.rs`

**Changes:**

```rust
// BEFORE:
pub struct MetadataStateMachine {
    pub nodes: HashMap<u64, String>,
    pub brokers: HashMap<i32, BrokerInfo>,
    pub partition_assignments: HashMap<PartitionKey, Vec<u64>>,
    pub partition_leaders: HashMap<PartitionKey, u64>,
    pub isr_sets: HashMap<PartitionKey, Vec<u64>>,
}

// AFTER:
pub struct MetadataStateMachine {
    // Cluster membership
    pub nodes: HashMap<u64, String>,
    pub brokers: HashMap<i32, BrokerInfo>,

    // Topic metadata (NEW)
    pub topics: HashMap<String, TopicMetadata>,

    // Partition metadata
    pub partition_assignments: HashMap<PartitionKey, Vec<u64>>,
    pub partition_leaders: HashMap<PartitionKey, u64>,
    pub isr_sets: HashMap<PartitionKey, Vec<u64>>,
    pub partition_high_watermarks: HashMap<PartitionKey, i64>,  // NEW
    pub partition_log_start_offsets: HashMap<PartitionKey, i64>, // NEW

    // Consumer metadata (NEW)
    pub consumer_groups: HashMap<String, ConsumerGroupMetadata>,
    pub consumer_offsets: HashMap<ConsumerOffsetKey, i64>,

    // Segment metadata (NEW - optional, may keep local)
    pub segments: HashMap<SegmentKey, SegmentMetadata>,
}

// New key types
pub type ConsumerOffsetKey = (String, String, u32); // (group_id, topic, partition)
pub type SegmentKey = (String, String); // (topic, segment_id)
```

**New commands:**

```rust
pub enum MetadataCommand {
    // Existing
    RegisterBroker { broker_id: i32, host: String, port: i32, rack: Option<String> },
    AssignPartition { topic: String, partition: u32, replicas: Vec<u64> },
    SetPartitionLeader { topic: String, partition: u32, leader: u64 },
    UpdateISR { topic: String, partition: u32, isr: Vec<u64> },

    // NEW: Topic management
    CreateTopic {
        name: String,
        partition_count: u32,
        replication_factor: u32,
        config: HashMap<String, String>,
    },
    DeleteTopic {
        name: String,
    },
    UpdateTopicConfig {
        name: String,
        config: HashMap<String, String>,
    },

    // NEW: Consumer group management
    CreateConsumerGroup {
        group_id: String,
        protocol_type: String,
        generation: i32,
    },
    UpdateConsumerGroup {
        group_id: String,
        generation: i32,
        members: Vec<String>,
    },
    DeleteConsumerGroup {
        group_id: String,
    },

    // NEW: Offset management
    CommitOffset {
        group_id: String,
        topic: String,
        partition: u32,
        offset: i64,
        metadata: Option<String>,
    },
    CommitOffsetBatch {
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>, // (topic, partition, offset, metadata)
    },

    // NEW: Partition offset tracking
    UpdatePartitionOffset {
        topic: String,
        partition: u32,
        high_watermark: i64,
        log_start_offset: i64,
    },
}
```

**Apply logic:**

```rust
impl MetadataStateMachine {
    pub fn apply(&mut self, cmd: &MetadataCommand) -> Result<()> {
        match cmd {
            // Existing implementations...

            MetadataCommand::CreateTopic { name, partition_count, replication_factor, config } => {
                let metadata = TopicMetadata {
                    id: Uuid::new_v4(),
                    name: name.clone(),
                    config: TopicConfig {
                        partition_count: *partition_count,
                        replication_factor: *replication_factor,
                        retention_ms: None,
                        segment_bytes: 100 * 1024 * 1024,
                        config: config.clone(),
                    },
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                self.topics.insert(name.clone(), metadata);
                Ok(())
            }

            MetadataCommand::CommitOffset { group_id, topic, partition, offset, metadata } => {
                let key = (group_id.clone(), topic.clone(), *partition);
                self.consumer_offsets.insert(key, *offset);
                Ok(())
            }

            MetadataCommand::CommitOffsetBatch { group_id, offsets } => {
                for (topic, partition, offset, _metadata) in offsets {
                    let key = (group_id.clone(), topic.clone(), *partition);
                    self.consumer_offsets.insert(key, *offset);
                }
                Ok(())
            }

            MetadataCommand::UpdatePartitionOffset { topic, partition, high_watermark, log_start_offset } => {
                let key = (topic.clone(), *partition);
                self.partition_high_watermarks.insert(key.clone(), *high_watermark);
                self.partition_log_start_offsets.insert(key, *log_start_offset);
                Ok(())
            }

            // Other commands...
        }
    }
}
```

**Snapshot support:**

```rust
impl MetadataStateMachine {
    pub fn snapshot(&self) -> Vec<u8> {
        bincode::serialize(self).expect("Failed to serialize state machine")
    }

    pub fn restore(&mut self, data: &[u8]) -> Result<()> {
        let state: MetadataStateMachine = bincode::deserialize(data)?;
        *self = state;
        Ok(())
    }
}
```

**Testing:**
```bash
cargo test --lib -p chronik-server test_raft_metadata_state_machine
```

---

### Phase 2: Implement RaftMetadataStore

**Objective:** Create MetadataStore implementation backed by Raft

**New file:** `crates/chronik-server/src/raft_metadata_store.rs`

```rust
use std::sync::Arc;
use async_trait::async_trait;
use chronik_common::metadata::{
    MetadataStore, MetadataError, Result,
    TopicConfig, TopicMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
};
use crate::raft_cluster::RaftCluster;
use crate::raft_metadata::MetadataCommand;

/// MetadataStore implementation backed by Raft state machine
///
/// All writes go through Raft consensus (propose → commit → apply)
/// All reads query Raft state machine directly (linearizable)
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
}

impl RaftMetadataStore {
    pub fn new(raft: Arc<RaftCluster>) -> Self {
        Self { raft }
    }

    /// Get read-only access to state machine
    fn state(&self) -> parking_lot::RwLockReadGuard<crate::raft_metadata::MetadataStateMachine> {
        self.raft.state_machine.read()
    }
}

#[async_trait]
impl MetadataStore for RaftMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Propose to Raft
        self.raft.propose(MetadataCommand::CreateTopic {
            name: name.to_string(),
            partition_count: config.partition_count,
            replication_factor: config.replication_factor,
            config: config.config.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        // Wait for apply (poll state machine)
        // TODO: Implement waiting mechanism (condition variable, watch channel, etc.)
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Read from state machine
        let state = self.state();
        state.topics.get(name)
            .cloned()
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found after creation", name)))
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let state = self.state();
        Ok(state.topics.get(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let state = self.state();
        Ok(state.topics.values().cloned().collect())
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        self.raft.propose(MetadataCommand::DeleteTopic {
            name: name.to_string(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        self.raft.propose(MetadataCommand::CommitOffset {
            group_id: offset.group_id.clone(),
            topic: offset.topic.clone(),
            partition: offset.partition,
            offset: offset.offset,
            metadata: offset.metadata.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let state = self.state();
        let key = (group_id.to_string(), topic.to_string(), partition);

        if let Some(&offset) = state.consumer_offsets.get(&key) {
            Ok(Some(ConsumerOffset {
                group_id: group_id.to_string(),
                topic: topic.to_string(),
                partition,
                offset,
                metadata: None,
                commit_timestamp: chrono::Utc::now(), // Approximate
            }))
        } else {
            Ok(None)
        }
    }

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        self.raft.propose(MetadataCommand::UpdatePartitionOffset {
            topic: topic.to_string(),
            partition,
            high_watermark,
            log_start_offset,
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let state = self.state();
        let key = (topic.to_string(), partition);

        let hw = state.partition_high_watermarks.get(&key).copied();
        let lso = state.partition_log_start_offsets.get(&key).copied();

        match (hw, lso) {
            (Some(hw), Some(lso)) => Ok(Some((hw, lso))),
            _ => Ok(None),
        }
    }

    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        self.raft.propose(MetadataCommand::RegisterBroker {
            broker_id: metadata.broker_id,
            host: metadata.host.clone(),
            port: metadata.port,
            rack: metadata.rack.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        let state = self.state();
        if let Some(broker_info) = state.brokers.get(&broker_id) {
            Ok(Some(BrokerMetadata {
                broker_id,
                host: broker_info.host.clone(),
                port: broker_info.port,
                rack: broker_info.rack.clone(),
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let state = self.state();
        let brokers = state.brokers.iter().map(|(&broker_id, broker_info)| {
            BrokerMetadata {
                broker_id,
                host: broker_info.host.clone(),
                port: broker_info.port,
                rack: broker_info.rack.clone(),
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }).collect();

        Ok(brokers)
    }

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        // Convert assignment to Raft command
        self.raft.propose(MetadataCommand::AssignPartition {
            topic: assignment.topic.clone(),
            partition: assignment.partition,
            replicas: vec![assignment.broker_id as u64], // Simplified
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        if assignment.is_leader {
            self.raft.propose(MetadataCommand::SetPartitionLeader {
                topic: assignment.topic.clone(),
                partition: assignment.partition,
                leader: assignment.broker_id as u64,
            }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;
        }

        Ok(())
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        let state = self.state();
        let key = (topic.to_string(), partition);
        Ok(state.partition_leaders.get(&key).map(|&id| id as i32))
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        let state = self.state();
        let key = (topic.to_string(), partition);
        Ok(state.partition_assignments.get(&key).map(|nodes| {
            nodes.iter().map(|&id| id as i32).collect()
        }))
    }

    // Implement remaining methods...
    // (consumer groups, transactions, etc.)
}
```

**Optimization: Wait for Apply**

```rust
// TODO: Implement proper wait mechanism
// Option 1: tokio::sync::watch channel (state machine version)
// Option 2: Condition variable
// Option 3: Poll with backoff

async fn wait_for_apply(&self, expected_version: u64, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let state = self.state();
        if state.version() >= expected_version {
            return Ok(());
        }
        drop(state);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(MetadataError::Timeout("Raft apply timeout".to_string()))
}
```

**Testing:**
```bash
cargo test --lib -p chronik-server test_raft_metadata_store
```

---

### Phase 3: Switch Cluster Mode to RaftMetadataStore

**Objective:** Use RaftMetadataStore for cluster deployments

**Files to modify:**
- `crates/chronik-server/src/integrated_server.rs`

**Changes:**

```rust
// BEFORE (lines 138-177):
let wal_adapter = Arc::new(WalMetadataAdapter::new(wal_config).await?);
let metalog_store = ChronikMetaLogStore::new(wal_adapter, snapshot_path).await?;
let metadata_store: Arc<dyn MetadataStore> = Arc::new(metalog_store);

// AFTER:
let metadata_store: Arc<dyn MetadataStore> = if let Some(ref raft) = raft_cluster {
    info!("Using RaftMetadataStore for cluster mode");
    Arc::new(RaftMetadataStore::new(raft.clone()))
} else {
    info!("Using ChronikMetaLogStore for standalone mode");
    let wal_adapter = Arc::new(WalMetadataAdapter::new(wal_config).await?);
    let metalog_store = ChronikMetaLogStore::new(wal_adapter, snapshot_path).await?;
    Arc::new(metalog_store)
};
```

**Remove local __meta topic creation (lines 178-203):**

```rust
// DELETE THIS BLOCK:
if metadata_store.get_topic(chronik_common::metadata::METADATA_TOPIC).await?.is_none() {
    info!("Creating internal metadata topic: {}", chronik_common::metadata::METADATA_TOPIC);
    // ... create __meta locally
}
```

**Testing:**
```bash
# Start cluster
./tests/cluster/start.sh

# Verify metadata consistency
python3 -c "
from kafka.admin import KafkaAdminClient

# Check all nodes
for port in [9092, 9093, 9094]:
    admin = KafkaAdminClient(bootstrap_servers=f'localhost:{port}')
    topics = admin.list_topics()
    print(f'Node {port}: {topics}')
    admin.close()
"
# Expected: All nodes return IDENTICAL topic lists
```

---

### Phase 4: System Topics via Raft

**Objective:** Create system topics through Raft consensus

**Files to modify:**
- `crates/chronik-server/src/integrated_server.rs`

**New function:**

```rust
/// Initialize system topics via Raft (cluster mode only)
async fn initialize_system_topics_raft(
    raft: &RaftCluster,
    metadata_store: &Arc<dyn MetadataStore>,
    replication_factor: u32,
) -> Result<()> {
    let system_topics = vec![
        ("__consumer_offsets", 50, replication_factor),
        ("__transaction_state", 50, replication_factor),
        ("__meta", 1, replication_factor),
    ];

    // Only leader should propose topic creation
    if !raft.is_leader() {
        info!("Not leader, waiting for system topics to be created by leader");
        // Wait for leader to create topics
        let mut retries = 0;
        while retries < 30 {
            let mut all_exist = true;
            for (name, _, _) in &system_topics {
                if metadata_store.get_topic(name).await?.is_none() {
                    all_exist = false;
                    break;
                }
            }

            if all_exist {
                info!("All system topics created by leader");
                return Ok(());
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            retries += 1;
        }

        return Err(anyhow::anyhow!("Timeout waiting for leader to create system topics"));
    }

    // Leader: create system topics
    info!("This node is leader, creating system topics");
    for (name, partitions, replication_factor) in system_topics {
        if metadata_store.get_topic(name).await?.is_none() {
            info!("Creating system topic: {} (partitions={}, rf={})", name, partitions, replication_factor);

            let config = TopicConfig {
                partition_count: partitions,
                replication_factor,
                retention_ms: None, // Never delete
                segment_bytes: 100 * 1024 * 1024,
                config: {
                    let mut cfg = std::collections::HashMap::new();
                    cfg.insert("compression.type".to_string(), "snappy".to_string());
                    cfg.insert("cleanup.policy".to_string(), "compact".to_string());
                    cfg
                },
            };

            // Propose to Raft
            metadata_store.create_topic(name, config).await?;
            info!("✓ Created system topic: {}", name);
        } else {
            info!("System topic {} already exists", name);
        }
    }

    Ok(())
}
```

**Call from IntegratedKafkaServer::new:**

```rust
// After broker registration, before handlers:
if let Some(ref raft) = raft_cluster {
    initialize_system_topics_raft(
        raft,
        &metadata_store,
        config.cluster_config.as_ref().unwrap().replication_factor as u32,
    ).await?;
}
```

**Testing:**
```bash
./tests/cluster/start.sh

# Check __meta topic on all nodes
python3 -c "
from kafka.admin import KafkaAdminClient
from kafka.structs import TopicPartition

for port in [9092, 9093, 9094]:
    admin = KafkaAdminClient(bootstrap_servers=f'localhost:{port}')
    metadata = admin._client.cluster
    metadata.request_update()

    import time; time.sleep(0.5)

    tp = TopicPartition('__meta', 0)
    leader = metadata.leader_for_partition(tp)

    print(f'Node {port}: __meta partition 0 leader = Node {leader}')
    admin.close()
"
# Expected: All nodes report SAME leader (Raft leader)
```

---

### Phase 5: Topic Auto-Creation via Raft

**Objective:** Regular topics created through Raft

**Files to modify:**
- `crates/chronik-server/src/produce_handler.rs`

**Changes:**

```rust
// BEFORE (lines 2104-2114):
let result = self.metadata_store.create_topic(topic_name, config).await?;
// ... then maybe propose to Raft

// AFTER:
let result = if self.raft_cluster.is_some() {
    // Cluster mode: Raft handles coordination
    self.metadata_store.create_topic(topic_name, config).await?
    // RaftMetadataStore already proposes to Raft internally
} else {
    // Standalone mode: direct creation
    self.metadata_store.create_topic(topic_name, config).await?
};

// Remove this block (Raft already handles partition initialization):
// if let Some(raft) = &self.raft_cluster {
//     self.initialize_raft_partitions(...).await?;
// }
```

**Partition assignment after topic creation:**

```rust
// After topic created via Raft, assign partitions
if self.raft_cluster.is_some() {
    // Cluster mode: round-robin across nodes
    let brokers = self.metadata_store.list_brokers().await?;
    let broker_ids: Vec<i32> = brokers.iter().map(|b| b.broker_id).collect();

    for partition in 0..config.partition_count {
        let leader_idx = (partition as usize) % broker_ids.len();
        let leader_id = broker_ids[leader_idx];

        let assignment = PartitionAssignment {
            topic: topic_name.to_string(),
            partition,
            broker_id: leader_id,
            is_leader: true,
        };

        self.metadata_store.assign_partition(assignment).await?;
    }
} else {
    // Standalone mode: all partitions to this node
    for partition in 0..config.partition_count {
        let assignment = PartitionAssignment {
            topic: topic_name.to_string(),
            partition,
            broker_id: self.node_id,
            is_leader: true,
        };

        self.metadata_store.assign_partition(assignment).await?;
    }
}
```

**Testing:**
```bash
# Start cluster
./tests/cluster/start.sh

# Create topic via producer
python3 -c "
from kafka import KafkaProducer

producer = KafkaProducer(bootstrap_servers='localhost:9092')
producer.send('auto-created-topic', b'test message')
producer.flush()
producer.close()

# Check all nodes see the topic
from kafka.admin import KafkaAdminClient

for port in [9092, 9093, 9094]:
    admin = KafkaAdminClient(bootstrap_servers=f'localhost:{port}')
    topics = admin.list_topics()
    assert 'auto-created-topic' in topics, f'Node {port} missing topic!'
    print(f'Node {port}: ✓ has auto-created-topic')
    admin.close()
"
```

---

### Phase 6: Consumer Offset Replication

**Objective:** Offset commits replicated via Raft

**Note:** Consumer offsets are now stored in Raft state machine via `CommitOffset` command.

**Files to modify:**
- `crates/chronik-server/src/coordinator_manager.rs` (offset commit handler)

**Optimization: Batch commits**

```rust
async fn handle_offset_commit_batch(
    &self,
    group_id: &str,
    offsets: Vec<(String, u32, i64, Option<String>)>,
) -> Result<()> {
    // Use batch command for performance
    if self.raft_cluster.is_some() {
        self.raft_cluster.as_ref().unwrap().propose(
            MetadataCommand::CommitOffsetBatch {
                group_id: group_id.to_string(),
                offsets,
            }
        ).await?;
    } else {
        // Standalone: individual commits
        for (topic, partition, offset, metadata) in offsets {
            let consumer_offset = ConsumerOffset {
                group_id: group_id.to_string(),
                topic,
                partition,
                offset,
                metadata,
                commit_timestamp: Utc::now(),
            };
            self.metadata_store.commit_offset(consumer_offset).await?;
        }
    }

    Ok(())
}
```

**Testing:**
```bash
# Start cluster
./tests/cluster/start.sh

# Commit offset on node 1
python3 -c "
from kafka import KafkaConsumer

consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9092',
    group_id='test-group',
    auto_offset_reset='earliest',
    enable_auto_commit=False,
)

# Consume a message
for msg in consumer:
    print(f'Consumed offset {msg.offset}')
    consumer.commit()
    break

consumer.close()
"

# Verify offset on node 2
python3 -c "
from kafka import KafkaConsumer

consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9093',  # Different node!
    group_id='test-group',
    enable_auto_commit=False,
)

# Check committed offset
partitions = consumer.partitions_for_topic('test-topic')
for partition in partitions:
    from kafka.structs import TopicPartition
    tp = TopicPartition('test-topic', partition)
    committed = consumer.committed(tp)
    print(f'Node 9093: Committed offset for partition {partition} = {committed}')
    assert committed is not None, 'Offset not replicated!'

consumer.close()
"
```

---

### Phase 7: Remove Bidirectional Sync

**Objective:** Eliminate sync workaround (no longer needed)

**Files to modify:**
- `crates/chronik-server/src/integrated_server.rs`

**Delete:**
- Lines 296-402: Bidirectional sync background task
- All `sync_raft_to_local` and `sync_local_to_raft` logic

**Reasoning:**
- RaftMetadataStore is single source of truth
- No local state to sync
- Raft handles all replication

---

### Phase 8: Comprehensive Testing

**Test Suite:**

1. **Basic cluster operations:**
```bash
cd /home/ubuntu/Development/chronik-stream
cargo test --test raft_cluster_metadata_consistency
```

2. **System topic consistency:**
```bash
./tests/cluster/start.sh
python3 tests/cluster/test_system_topics.py
```

3. **Topic auto-creation:**
```bash
python3 tests/cluster/test_topic_autocreate.py
```

4. **Consumer offset replication:**
```bash
python3 tests/cluster/test_offset_replication.py
```

5. **Failover scenarios:**
```bash
python3 tests/cluster/test_leader_failover.py
```

6. **Split-brain prevention:**
```bash
python3 tests/cluster/test_network_partition.py
```

**New test files to create:**

`tests/cluster/test_system_topics.py`:
```python
"""Test that system topics are consistent across all nodes."""
from kafka.admin import KafkaAdminClient
from kafka.structs import TopicPartition
import time

def test_system_topics_consistency():
    """Verify __meta topic has same leader on all nodes."""
    nodes = [9092, 9093, 9094]
    leaders = {}

    for port in nodes:
        admin = KafkaAdminClient(bootstrap_servers=f'localhost:{port}')
        metadata = admin._client.cluster
        metadata.request_update()
        time.sleep(0.5)

        tp = TopicPartition('__meta', 0)
        leader = metadata.leader_for_partition(tp)
        leaders[port] = leader

        admin.close()

    # All nodes must report same leader
    unique_leaders = set(leaders.values())
    assert len(unique_leaders) == 1, f"Split-brain detected: {leaders}"
    print(f"✓ All nodes agree: __meta leader = Node {unique_leaders.pop()}")

if __name__ == '__main__':
    test_system_topics_consistency()
```

`tests/cluster/test_offset_replication.py`:
```python
"""Test consumer offset replication across nodes."""
from kafka import KafkaProducer, KafkaConsumer
from kafka.structs import TopicPartition
import time

def test_offset_replication():
    """Commit offset on one node, verify on another."""
    # Produce test message
    producer = KafkaProducer(bootstrap_servers='localhost:9092')
    producer.send('offset-test', b'message 1')
    producer.send('offset-test', b'message 2')
    producer.send('offset-test', b'message 3')
    producer.flush()
    producer.close()

    # Consume on node 1, commit offset
    consumer1 = KafkaConsumer(
        'offset-test',
        bootstrap_servers='localhost:9092',
        group_id='offset-test-group',
        auto_offset_reset='earliest',
        enable_auto_commit=False,
    )

    messages = []
    for msg in consumer1:
        messages.append(msg.offset)
        if len(messages) == 2:
            break

    consumer1.commit()
    committed_offset = messages[-1] + 1
    print(f"Node 1: Committed offset {committed_offset}")
    consumer1.close()

    # Wait for replication
    time.sleep(2)

    # Verify on node 2
    consumer2 = KafkaConsumer(
        'offset-test',
        bootstrap_servers='localhost:9093',  # Different node!
        group_id='offset-test-group',
        enable_auto_commit=False,
    )

    tp = TopicPartition('offset-test', 0)
    offset_on_node2 = consumer2.committed(tp)
    print(f"Node 2: Sees committed offset {offset_on_node2}")

    assert offset_on_node2 == committed_offset, \
        f"Offset not replicated! Node 1={committed_offset}, Node 2={offset_on_node2}"

    print("✓ Offset successfully replicated across nodes")
    consumer2.close()

if __name__ == '__main__':
    test_offset_replication()
```

---

### Phase 9: Migration & Backward Compatibility

**Standalone Mode:**
- No changes required
- ChronikMetaLogStore continues to work

**Cluster Mode:**
- v2.2.x → v2.3.0 migration requires fresh bootstrap
- Existing clusters: Stop all nodes, delete data, re-bootstrap

**Migration script:**
```bash
#!/bin/bash
# migrate_to_v2.3.0.sh

set -e

echo "Migrating Chronik cluster to v2.3.0 (Unified Metadata)"
echo "WARNING: This will DELETE all data! Backup first!"
read -p "Continue? (yes/no): " confirm

if [ "$confirm" != "yes" ]; then
    echo "Aborted"
    exit 1
fi

# Stop cluster
./tests/cluster/stop.sh

# Backup data
backup_dir="backup-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$backup_dir"
cp -r tests/cluster/data "$backup_dir/"
echo "Backup saved to $backup_dir"

# Clean data directories
rm -rf tests/cluster/data/node{1,2,3}/*
rm -rf tests/cluster/logs/*.log

# Rebuild server with new version
cargo build --release --bin chronik-server

# Start cluster with fresh bootstrap
./tests/cluster/start.sh

echo "✓ Migration complete"
echo "Note: All topics and offsets lost - this is a fresh cluster"
```

**Future: Snapshot Import**
```rust
// TODO v2.4.0: Implement snapshot import from ChronikMetaLogStore
pub async fn import_metalog_snapshot(
    raft: &RaftCluster,
    snapshot_path: &Path,
) -> Result<()> {
    // 1. Read ChronikMetaLogStore snapshot
    // 2. Convert to Raft state machine format
    // 3. Propose all state via Raft
    // 4. Wait for apply
    // 5. Verify consistency
}
```

---

## Performance Considerations

### Raft Write Latency

**Problem:** Every metadata write goes through Raft consensus (2-RTT)

**Mitigations:**
1. **Batch commits:** Group multiple offset commits into one proposal
2. **Async propose:** Don't wait for apply for non-critical writes
3. **Local caching:** Cache reads in state machine (no Raft round-trip)
4. **Leadership stickiness:** Keep clients connected to leader (avoid redirects)

### Offset Commit Throughput

**Benchmark target:** 10,000 offset commits/sec

**Optimization:**
```rust
// Batch offset commits per consumer group
pub struct OffsetCommitBatcher {
    pending: HashMap<String, Vec<(String, u32, i64)>>,
    flush_interval: Duration,
}

impl OffsetCommitBatcher {
    async fn flush(&mut self, raft: &RaftCluster) {
        for (group_id, offsets) in self.pending.drain() {
            raft.propose(MetadataCommand::CommitOffsetBatch {
                group_id,
                offsets,
            }).await.ok();
        }
    }
}
```

### State Machine Size

**Growth:** 1 MB per 10,000 topics, 100 MB per 1,000,000 offsets

**Mitigations:**
1. **Snapshots:** Every 100,000 entries
2. **Log compaction:** Keep only latest offset per partition
3. **Offset expiration:** Delete old consumer groups (configurable TTL)

---

## Rollout Plan

### Development (Week 1)

- [ ] Implement Phase 1: Expand MetadataStateMachine
- [ ] Implement Phase 2: RaftMetadataStore
- [ ] Unit tests for RaftMetadataStore

### Testing (Week 2)

- [ ] Implement Phase 3: Switch cluster mode
- [ ] Implement Phase 4: System topics via Raft
- [ ] Implement Phase 5: Topic auto-creation
- [ ] Implement Phase 6: Offset replication
- [ ] Comprehensive test suite

### Validation (Week 3)

- [ ] Phase 7: Remove bidirectional sync
- [ ] Phase 8: Full integration testing
- [ ] Performance benchmarks
- [ ] Chaos testing (network partitions, crashes)

### Release (Week 4)

- [ ] Phase 9: Migration documentation
- [ ] Update CLAUDE.md
- [ ] Release v2.3.0-rc1
- [ ] Community testing
- [ ] Release v2.3.0 stable

---

## Success Criteria

1. **Metadata consistency:** All nodes return identical metadata for topics, offsets, brokers
2. **System topics:** `__meta` created on Raft leader only, replicated to all nodes
3. **Offset durability:** Committed offsets survive node failures
4. **Topic visibility:** Topics created on any node visible on all nodes within 1 second
5. **Performance:** < 10ms p99 latency for metadata reads, < 100ms for writes
6. **Backward compatible:** Standalone mode unchanged, cluster mode fresh bootstrap

---

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Raft write latency | Slow offset commits | Batch commits, async propose |
| State machine bloat | Memory usage | Snapshots, compaction, TTL |
| Migration complexity | Deployment friction | Fresh bootstrap, clear docs |
| Regression in standalone | Broken single-node | Keep ChronikMetaLogStore, thorough testing |
| Incomplete replication | Split-brain persists | Comprehensive integration tests |

---

## Open Questions

1. **Should segment metadata be in Raft?**
   - Pro: Full replication
   - Con: High write volume, large state
   - Recommendation: Keep local, sync on-demand

2. **Linearizable reads vs leader leases?**
   - Current: Read from state machine (stale on followers)
   - Alternative: Forward reads to leader (stronger consistency)
   - Recommendation: Start with local reads, add `read_quorum=true` flag later

3. **Offset commit acknowledgment?**
   - Current: Wait for Raft apply (slow)
   - Alternative: Ack on leader accept (faster, weaker guarantee)
   - Recommendation: Configurable `acks` parameter (all, leader, none)

---

## Summary

This plan eliminates Chronik's dual metadata architecture by:

1. **Expanding Raft state machine** to include ALL metadata
2. **Implementing RaftMetadataStore** as single source of truth
3. **Switching cluster mode** to use Raft-backed metadata
4. **Keeping standalone mode** unchanged (ChronikMetaLogStore)
5. **Comprehensive testing** to prevent regressions

**Result:** Strongly consistent metadata, no split-brain, production-ready clustering.

**Estimated effort:** 3-4 weeks (1 week dev, 2 weeks testing, 1 week validation)

**Target release:** v2.3.0 (Q1 2025)
